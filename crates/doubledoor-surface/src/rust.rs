//! Rust symbol extraction (§10, §47 Phase III): parse one source file with
//! `syn` and produce its symbols (items, members, signatures, body hashes)
//! and its outgoing references (paths, method calls, field accesses, struct
//! literals, exhaustive matches, trait impls), each attributed to the
//! enclosing symbol.
//!
//! This is AST evidence (§17, level 2): deterministic, but syntactic. Name
//! resolution follows the file's `use` declarations and `crate`/`self`/
//! `super`/`Self` prefixes; it does not run the compiler, so it neither
//! resolves glob imports nor type-directed method dispatch. Every consumer
//! labels the evidence accordingly and resolves ambiguity conservatively
//! (a name match counts as a reference).
//!
//! Parser exhaustion (§27): sources above [`MAX_RUST_SOURCE_BYTES`] are not
//! parsed, and a lexical pre-scan rejects bracket nesting deeper than
//! [`MAX_NESTING`] (and absurd unary-operator chains) before `syn`'s
//! recursive descent runs — a stack overflow would abort the whole process,
//! so it must never be reachable from repository content. Parsing then runs
//! on a dedicated thread with a large fixed stack.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;
use serde::{Deserialize, Serialize};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Expr, Fields, ImplItem, Item, Pat, Token, TraitItem, Type, UseTree, Visibility};

/// Largest source file the analyzer will parse.
pub const MAX_RUST_SOURCE_BYTES: usize = 1024 * 1024;

/// Stack size of the parser thread. Generous headroom for legitimate code;
/// the nesting pre-scan (not the stack) is what stops hostile input, because
/// a stack overflow aborts the process rather than failing the thread.
const PARSER_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Deepest bracket nesting the parser will accept.
pub const MAX_NESTING: usize = 256;

/// Longest run of adjacent prefix operators (`!!!!x`, `- - - x`) accepted.
const MAX_UNARY_RUN: usize = 256;

/// The surface-id prefix of Rust symbols: `rust:<qualified path>`.
pub const RUST_ID_PREFIX: &str = "rust:";

/// What kind of item a symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    /// An inherent or trait-impl method / associated fn, const, or type.
    Method,
    Struct,
    Union,
    /// A named or positional struct/union field (`Type.field`).
    Field,
    Enum,
    /// An enum variant (`Enum::Variant`).
    Variant,
    Trait,
    /// A trait's required or provided method, const, or type.
    TraitItem,
    TypeAlias,
    Const,
    Static,
    Module,
    Macro,
}

impl SymbolKind {
    /// Member kinds live under an owner (`Type.field`, `Enum::Variant`, ...).
    pub fn is_member(self) -> bool {
        matches!(
            self,
            SymbolKind::Method | SymbolKind::Field | SymbolKind::Variant | SymbolKind::TraitItem
        )
    }
}

/// Declared visibility, coarsened to what matters for API impact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SymbolVisibility {
    Private,
    /// `pub(crate)`, `pub(super)`, `pub(in path)`.
    Restricted,
    Public,
}

/// One symbol defined in a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    /// `[crate, module..., item]`, plus the member for member kinds.
    pub path: Vec<String>,
    pub kind: SymbolKind,
    pub visibility: SymbolVisibility,
    /// Normalized API-relevant tokens: a fn's full signature, a field's
    /// type, a variant's shape, a struct's generics. Comments and
    /// formatting never affect it.
    pub signature: String,
    /// FNV-1a of the item's full tokens with doc attributes removed:
    /// changes when the implementation changes.
    pub body_hash: u64,
    /// For trait items: whether a default body exists.
    pub has_default: bool,
    /// 1-based source line of the definition.
    pub line: usize,
}

impl Symbol {
    /// Human/qualified rendering: `app::router::dispatch`,
    /// `app::config::RouterConfig.retry_limit`, `app::Outcome::Retry`.
    pub fn qualified(&self) -> String {
        qualify(&self.path, self.kind)
    }

    /// The symbol's surface id: `rust:<qualified>`.
    pub fn surface_id(&self) -> doubledoor_core::SurfaceId {
        doubledoor_core::SurfaceId::new(format!("{RUST_ID_PREFIX}{}", self.qualified()))
            .expect("qualified symbol paths are valid surface ids")
    }

    /// Last path segment (the item or member name).
    pub fn name(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or_default()
    }

    /// The owner path of a member (`[.., RouterConfig]` for a field).
    pub fn owner(&self) -> Option<&[String]> {
        if self.kind.is_member() && self.path.len() >= 2 {
            Some(&self.path[..self.path.len() - 1])
        } else {
            None
        }
    }
}

/// Render a symbol path; fields use `.`, everything else `::`.
pub fn qualify(path: &[String], kind: SymbolKind) -> String {
    if kind == SymbolKind::Field && path.len() >= 2 {
        format!(
            "{}.{}",
            path[..path.len() - 1].join("::"),
            path[path.len() - 1]
        )
    } else {
        path.join("::")
    }
}

/// What a reference points at, after syntactic name resolution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RefTarget {
    /// A path in expression, type, or pattern position (`dispatch`,
    /// `router::dispatch`, `RouterConfig`, `Outcome::Retry`).
    Path(Vec<String>),
    /// `.name(..)` method-call syntax (receiver type unknown).
    Method(String),
    /// `.name` field access (receiver type unknown).
    Field(String),
    /// `Path { .. }` construction; `has_rest` for `..base` functional update.
    StructLiteral { path: Vec<String>, has_rest: bool },
    /// A `match` whose arms name the enum's variants with no catch-all arm.
    ExhaustiveMatch { enum_path: Vec<String> },
    /// `impl Trait for Type`.
    TraitImpl { trait_path: Vec<String> },
}

/// One outgoing reference from a file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Reference {
    /// Qualified name of the enclosing symbol (or the module for top-level
    /// uses outside any item).
    pub from: String,
    pub target: RefTarget,
    pub line: usize,
}

/// Everything extracted from one file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSymbols {
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
    /// First segments of external paths (`serde`, `tokio`, ...) — candidate
    /// crate dependencies of the file.
    pub external_roots: BTreeSet<String>,
}

/// Why a file could not be analyzed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RustParseError {
    #[error("source is {bytes} bytes, above the {max}-byte analysis cap")]
    Oversized { bytes: usize, max: usize },
    #[error("syntax error at line {line}: {message}")]
    Syntax { line: usize, message: String },
    #[error("nesting depth {depth} exceeds the analysis limit of {max}")]
    TooDeep { depth: usize, max: usize },
    #[error("parser thread failed")]
    Exhausted,
}

/// Parse `source` as the file at module path `module` (`[crate, mods...]`)
/// and extract its symbols and references.
pub fn analyze_source(source: &str, module: &[String]) -> Result<FileSymbols, RustParseError> {
    let module = module.to_vec();
    with_parsed_file(source, move |file| extract(file, &module))
}

/// Parse `source` under the §27 guards (size cap, nesting pre-scan, a
/// dedicated large-stack thread) and run `f` on the syntax tree there.
/// `syn` trees are not `Send`, so callers extract what they need inside `f`.
pub fn with_parsed_file<T, F>(source: &str, f: F) -> Result<T, RustParseError>
where
    F: FnOnce(&syn::File) -> T + Send + 'static,
    T: Send + 'static,
{
    if source.len() > MAX_RUST_SOURCE_BYTES {
        return Err(RustParseError::Oversized {
            bytes: source.len(),
            max: MAX_RUST_SOURCE_BYTES,
        });
    }
    let depth = nesting_depth(source);
    if depth > MAX_NESTING {
        return Err(RustParseError::TooDeep {
            depth,
            max: MAX_NESTING,
        });
    }
    let owned = source.to_string();
    let handle = std::thread::Builder::new()
        .name("doubledoor-rust-parse".to_string())
        .stack_size(PARSER_STACK_BYTES)
        .spawn(move || -> Result<T, RustParseError> {
            let file = syn::parse_file(&owned).map_err(|e| RustParseError::Syntax {
                line: e.span().start().line,
                message: e.to_string(),
            })?;
            Ok(f(&file))
        })
        .map_err(|_| RustParseError::Exhausted)?;
    handle.join().unwrap_or(Err(RustParseError::Exhausted))
}

/// Upper bound on the recursion depth `syn` would need: the deepest bracket
/// nesting, or the longest prefix-operator run, whichever is larger (the
/// run is scaled so both compare against [`MAX_NESTING`]). String, char, and
/// comment contents are skipped; when the scan is unsure it over-counts,
/// which only ever rejects input.
pub fn nesting_depth(src: &str) -> usize {
    let b = src.as_bytes();
    let (mut i, mut depth, mut max, mut run, mut max_run) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    while i < b.len() {
        let c = b[i];
        match c {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut level = 0usize;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        level += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        level -= 1;
                        i += 2;
                        if level == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            b'r' if matches!(b.get(i + 1), Some(b'"') | Some(b'#'))
                && (i == 0 || !is_ident_byte(b[i - 1]) || b[i - 1] == b'b') =>
            {
                // Raw string r#*"..."#*.
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    j += 1;
                    'raw: while j < b.len() {
                        if b[j] == b'"' && (0..hashes).all(|k| b.get(j + 1 + k) == Some(&b'#')) {
                            j += 1 + hashes;
                            break 'raw;
                        }
                        j += 1;
                    }
                    i = j;
                    run = 0;
                    continue;
                }
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                run = 0;
                continue;
            }
            b'\'' => {
                // Char literal ('x', '\n', '\u{..}') vs lifetime ('a).
                if b.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                let ch_len = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                if b.get(i + 1 + ch_len) == Some(&b'\'') {
                    i += 2 + ch_len;
                    continue;
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if matches!(c, b'!' | b'-' | b'*' | b'&' | b'|') {
            run += 1;
            max_run = max_run.max(run);
        } else if !c.is_ascii_whitespace() {
            run = 0;
        }
        i += 1;
    }
    max.max(max_run * MAX_NESTING / MAX_UNARY_RUN)
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

fn extract(file: &syn::File, module: &[String]) -> FileSymbols {
    let crate_name = module
        .first()
        .cloned()
        .unwrap_or_else(|| "crate".to_string());
    let mut imports = BTreeMap::new();
    collect_imports(&file.items, module, &crate_name, &mut imports);
    let mut out = FileSymbols::default();
    for full in imports.values() {
        if let Some(root) = full.first() {
            if root != &crate_name && !module.contains(root) {
                out.external_roots.insert(root.clone());
            }
        }
    }
    let mut ctx = Ctx {
        crate_name,
        imports,
        out: &mut out,
    };
    ctx.items(&file.items, module);
    out.symbols
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    out.symbols
        .dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
    out.references.sort();
    out.references.dedup();
    out
}

struct Ctx<'o> {
    crate_name: String,
    /// Local name → normalized full path, from every `use` in the file.
    imports: BTreeMap<String, Vec<String>>,
    out: &'o mut FileSymbols,
}

impl Ctx<'_> {
    fn items(&mut self, items: &[Item], module: &[String]) {
        for item in items {
            self.item(item, module);
        }
    }

    fn push(
        &mut self,
        path: Vec<String>,
        kind: SymbolKind,
        vis: SymbolVisibility,
        signature: String,
        body: &dyn ToTokens,
        span: Span,
    ) {
        // Containers' members are symbols of their own; the container is
        // identified by its signature alone, so adding a field is reported
        // once (as the field), not again as a container body change.
        let body_hash = if matches!(
            kind,
            SymbolKind::Struct | SymbolKind::Union | SymbolKind::Enum | SymbolKind::Trait
        ) {
            fnv1a(&signature)
        } else {
            fnv1a(&tokens_without_docs(body))
        };
        self.out.symbols.push(Symbol {
            path,
            kind,
            visibility: vis,
            signature,
            body_hash,
            has_default: false,
            line: span.start().line,
        });
    }

    fn refs(
        &mut self,
        from: &[String],
        kind: SymbolKind,
        node: &dyn Visitable,
        module: &[String],
        self_ty: Option<&[String]>,
    ) {
        let mut v = RefVisitor {
            from: qualify(from, kind),
            module: module.to_vec(),
            self_ty: self_ty.map(<[String]>::to_vec),
            crate_name: &self.crate_name,
            imports: &self.imports,
            out: self.out,
        };
        node.accept(&mut v);
    }

    fn item(&mut self, item: &Item, module: &[String]) {
        match item {
            Item::Fn(f) => {
                let path = child(module, &f.sig.ident.to_string());
                self.push(
                    path.clone(),
                    SymbolKind::Function,
                    vis_of(&f.vis),
                    tok(&f.sig),
                    f,
                    f.sig.ident.span(),
                );
                self.refs(&path, SymbolKind::Function, f, module, None);
            }
            Item::Struct(s) => {
                let path = child(module, &s.ident.to_string());
                let shape = match &s.fields {
                    Fields::Named(_) => "named",
                    Fields::Unnamed(_) => "tuple",
                    Fields::Unit => "unit",
                };
                self.push(
                    path.clone(),
                    SymbolKind::Struct,
                    vis_of(&s.vis),
                    format!("{}{shape} {}", attrs_sig(&s.attrs), tok(&s.generics)),
                    s,
                    s.ident.span(),
                );
                self.fields(&path, &s.fields);
                self.refs(&path, SymbolKind::Struct, s, module, Some(&path));
            }
            Item::Union(u) => {
                let path = child(module, &u.ident.to_string());
                self.push(
                    path.clone(),
                    SymbolKind::Union,
                    vis_of(&u.vis),
                    format!("{}{}", attrs_sig(&u.attrs), tok(&u.generics)),
                    u,
                    u.ident.span(),
                );
                self.fields(&path, &Fields::Named(u.fields.clone()));
                self.refs(&path, SymbolKind::Union, u, module, Some(&path));
            }
            Item::Enum(e) => {
                let path = child(module, &e.ident.to_string());
                self.push(
                    path.clone(),
                    SymbolKind::Enum,
                    vis_of(&e.vis),
                    format!("{}{}", attrs_sig(&e.attrs), tok(&e.generics)),
                    e,
                    e.ident.span(),
                );
                for v in &e.variants {
                    let disc = v
                        .discriminant
                        .as_ref()
                        .map(|(_, d)| format!(" = {}", tok(d)))
                        .unwrap_or_default();
                    self.push(
                        child(&path, &v.ident.to_string()),
                        SymbolKind::Variant,
                        vis_of(&e.vis),
                        format!("{}{disc}", tok(&v.fields)),
                        v,
                        v.ident.span(),
                    );
                }
                self.refs(&path, SymbolKind::Enum, e, module, Some(&path));
            }
            Item::Trait(t) => {
                let path = child(module, &t.ident.to_string());
                let supers: Vec<String> = t.supertraits.iter().map(|b| tok(b)).collect();
                self.push(
                    path.clone(),
                    SymbolKind::Trait,
                    vis_of(&t.vis),
                    format!(
                        "{}trait {}: {}",
                        if t.unsafety.is_some() { "unsafe " } else { "" },
                        tok(&t.generics),
                        supers.join(" + ")
                    ),
                    t,
                    t.ident.span(),
                );
                let vis = vis_of(&t.vis);
                for ti in &t.items {
                    let (name, sig, has_default, span) = match ti {
                        TraitItem::Fn(f) => (
                            f.sig.ident.to_string(),
                            tok(&f.sig),
                            f.default.is_some(),
                            f.sig.ident.span(),
                        ),
                        TraitItem::Const(c) => (
                            c.ident.to_string(),
                            format!("const {}", tok(&c.ty)),
                            c.default.is_some(),
                            c.ident.span(),
                        ),
                        TraitItem::Type(ty) => {
                            let bounds: Vec<String> = ty.bounds.iter().map(|b| tok(b)).collect();
                            (
                                ty.ident.to_string(),
                                format!("type {}: {}", tok(&ty.generics), bounds.join(" + ")),
                                ty.default.is_some(),
                                ty.ident.span(),
                            )
                        }
                        _ => continue,
                    };
                    let mpath = child(&path, &name);
                    self.push(mpath, SymbolKind::TraitItem, vis, sig, ti, span);
                    if let Some(last) = self.out.symbols.last_mut() {
                        last.has_default = has_default;
                    }
                }
                self.refs(&path, SymbolKind::Trait, t, module, Some(&path));
            }
            Item::Impl(imp) => self.impl_block(imp, module),
            Item::Type(t) => {
                let path = child(module, &t.ident.to_string());
                self.push(
                    path.clone(),
                    SymbolKind::TypeAlias,
                    vis_of(&t.vis),
                    format!("{} = {}", tok(&t.generics), tok(&t.ty)),
                    t,
                    t.ident.span(),
                );
                self.refs(&path, SymbolKind::TypeAlias, t, module, None);
            }
            Item::Const(c) => {
                let path = child(module, &c.ident.to_string());
                self.push(
                    path.clone(),
                    SymbolKind::Const,
                    vis_of(&c.vis),
                    tok(&c.ty),
                    c,
                    c.ident.span(),
                );
                self.refs(&path, SymbolKind::Const, c, module, None);
            }
            Item::Static(s) => {
                let path = child(module, &s.ident.to_string());
                let m = if matches!(s.mutability, syn::StaticMutability::Mut(_)) {
                    "mut "
                } else {
                    ""
                };
                self.push(
                    path.clone(),
                    SymbolKind::Static,
                    vis_of(&s.vis),
                    format!("{m}{}", tok(&s.ty)),
                    s,
                    s.ident.span(),
                );
                self.refs(&path, SymbolKind::Static, s, module, None);
            }
            Item::Mod(m) => {
                let path = child(module, &m.ident.to_string());
                if let Some((_, items)) = &m.content {
                    self.push(
                        path.clone(),
                        SymbolKind::Module,
                        vis_of(&m.vis),
                        String::new(),
                        &TokenStream::new(),
                        m.ident.span(),
                    );
                    self.items(items, &path);
                } else {
                    // `mod x;` declares a file-backed module; its symbols come
                    // from that file. The declaration itself is the surface.
                    self.push(
                        path,
                        SymbolKind::Module,
                        vis_of(&m.vis),
                        String::new(),
                        &TokenStream::new(),
                        m.ident.span(),
                    );
                }
            }
            Item::Macro(m) => {
                if let Some(ident) = &m.ident {
                    let path = child(module, &ident.to_string());
                    self.push(
                        path,
                        SymbolKind::Macro,
                        SymbolVisibility::Private,
                        String::new(),
                        m,
                        ident.span(),
                    );
                } else {
                    // A macro invocation at item level (e.g. `lazy_static!`):
                    // its contents still reference things.
                    let from = module.to_vec();
                    self.refs(&from, SymbolKind::Module, m, module, None);
                }
            }
            _ => {}
        }
    }

    fn fields(&mut self, owner: &[String], fields: &Fields) {
        for (i, f) in fields.iter().enumerate() {
            let name = f
                .ident
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| i.to_string());
            let span = f
                .ident
                .as_ref()
                .map(|i| i.span())
                .unwrap_or_else(|| f.ty.span_start());
            self.push(
                child(owner, &name),
                SymbolKind::Field,
                vis_of(&f.vis),
                tok(&f.ty),
                f,
                span,
            );
        }
    }

    fn impl_block(&mut self, imp: &syn::ItemImpl, module: &[String]) {
        let Some(self_name) = type_name(&imp.self_ty) else {
            return;
        };
        let self_path = self.resolve(std::slice::from_ref(&self_name), module, None);
        let trait_path = imp
            .trait_
            .as_ref()
            .map(|(_, p, _)| self.resolve(&path_segments(p), module, None));
        if let Some(tp) = &trait_path {
            let line = imp.impl_token.span.start().line;
            self.out.references.push(Reference {
                from: self_path.join("::"),
                target: RefTarget::TraitImpl {
                    trait_path: tp.clone(),
                },
                line,
            });
        }
        // Inherent members live at `Type::member`; trait-impl members at
        // `<Type as Trait>::member` so they never collide with inherent ones.
        let owner = match &trait_path {
            None => child(module, &self_name),
            Some(tp) => child(
                module,
                &format!(
                    "<{self_name} as {}>",
                    tp.last().cloned().unwrap_or_default()
                ),
            ),
        };
        let trait_vis = trait_path.is_some();
        for ii in &imp.items {
            let (name, vis, sig, span) = match ii {
                ImplItem::Fn(f) => (
                    f.sig.ident.to_string(),
                    &f.vis,
                    tok(&f.sig),
                    f.sig.ident.span(),
                ),
                ImplItem::Const(c) => (
                    c.ident.to_string(),
                    &c.vis,
                    format!("const {}", tok(&c.ty)),
                    c.ident.span(),
                ),
                ImplItem::Type(t) => (
                    t.ident.to_string(),
                    &t.vis,
                    format!("type {} = {}", tok(&t.generics), tok(&t.ty)),
                    t.ident.span(),
                ),
                _ => continue,
            };
            let path = child(&owner, &name);
            // Trait-impl members are as visible as the trait itself.
            let vis = if trait_vis {
                SymbolVisibility::Public
            } else {
                vis_of(vis)
            };
            self.push(path.clone(), SymbolKind::Method, vis, sig, ii, span);
            self.refs(&path, SymbolKind::Method, ii, module, Some(&self_path));
        }
        // Where-clauses and the self type reference things too.
        let from = owner.clone();
        self.refs(
            &from,
            SymbolKind::Module,
            imp.self_ty.as_ref(),
            module,
            Some(&self_path),
        );
        self.refs(
            &from,
            SymbolKind::Module,
            &imp.generics,
            module,
            Some(&self_path),
        );
    }

    fn resolve(
        &self,
        segs: &[String],
        module: &[String],
        self_ty: Option<&[String]>,
    ) -> Vec<String> {
        resolve(segs, module, self_ty, &self.crate_name, &self.imports)
    }
}

/// Normalize a path: `crate`/`self`/`super`/`Self` prefixes and imported
/// names expand to full `[crate, module..., item]` paths.
fn resolve(
    segs: &[String],
    module: &[String],
    self_ty: Option<&[String]>,
    crate_name: &str,
    imports: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let Some(first) = segs.first() else {
        return Vec::new();
    };
    let rest = &segs[1..];
    let mut base: Vec<String> = match first.as_str() {
        "crate" => vec![crate_name.to_string()],
        "self" => module.to_vec(),
        "super" => {
            let mut m = module.to_vec();
            let mut rest_iter = rest.iter().peekable();
            m.pop();
            while rest_iter.peek().is_some_and(|s| s.as_str() == "super") {
                rest_iter.next();
                m.pop();
            }
            m.extend(rest_iter.cloned());
            return m;
        }
        "Self" => match self_ty {
            Some(t) => t.to_vec(),
            None => vec![first.clone()],
        },
        _ => match imports.get(first) {
            Some(full) => full.clone(),
            None => vec![first.clone()],
        },
    };
    base.extend(rest.iter().cloned());
    base
}

/// Record every `use` in the file (including nested modules) as
/// local-name → normalized path. Glob imports are not expanded.
fn collect_imports(
    items: &[Item],
    module: &[String],
    crate_name: &str,
    out: &mut BTreeMap<String, Vec<String>>,
) {
    for item in items {
        match item {
            Item::Use(u) => walk_use(&u.tree, &mut Vec::new(), module, crate_name, out),
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    collect_imports(items, &child(module, &m.ident.to_string()), crate_name, out);
                }
            }
            _ => {}
        }
    }
}

fn walk_use(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    module: &[String],
    crate_name: &str,
    out: &mut BTreeMap<String, Vec<String>>,
) {
    let empty = BTreeMap::new();
    match tree {
        UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            walk_use(&p.tree, prefix, module, crate_name, out);
            prefix.pop();
        }
        UseTree::Name(n) => {
            let name = n.ident.to_string();
            let mut full = prefix.clone();
            if name != "self" {
                full.push(name.clone());
            }
            let local = if name == "self" {
                prefix.last().cloned().unwrap_or_default()
            } else {
                name
            };
            out.insert(local, resolve(&full, module, None, crate_name, &empty));
        }
        UseTree::Rename(r) => {
            let mut full = prefix.clone();
            if r.ident != "self" {
                full.push(r.ident.to_string());
            }
            out.insert(
                r.rename.to_string(),
                resolve(&full, module, None, crate_name, &empty),
            );
        }
        UseTree::Group(g) => {
            for t in &g.items {
                walk_use(t, prefix, module, crate_name, out);
            }
        }
        UseTree::Glob(_) => {}
    }
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// Nodes a [`RefVisitor`] can walk.
trait Visitable {
    fn accept(&self, v: &mut RefVisitor<'_, '_>);
}

macro_rules! visitable {
    ($($ty:ty => $method:ident),* $(,)?) => {
        $(impl Visitable for $ty {
            fn accept(&self, v: &mut RefVisitor<'_, '_>) {
                v.$method(self);
            }
        })*
    };
}

visitable!(
    syn::ItemFn => visit_item_fn,
    syn::ItemStruct => visit_item_struct,
    syn::ItemUnion => visit_item_union,
    syn::ItemEnum => visit_item_enum,
    syn::ItemTrait => visit_item_trait,
    syn::ItemType => visit_item_type,
    syn::ItemConst => visit_item_const,
    syn::ItemStatic => visit_item_static,
    syn::ItemMacro => visit_item_macro,
    syn::ImplItem => visit_impl_item,
    Type => visit_type,
    syn::Generics => visit_generics,
);

struct RefVisitor<'a, 'o> {
    from: String,
    module: Vec<String>,
    self_ty: Option<Vec<String>>,
    crate_name: &'a str,
    imports: &'a BTreeMap<String, Vec<String>>,
    out: &'o mut FileSymbols,
}

impl RefVisitor<'_, '_> {
    fn resolved(&self, segs: &[String]) -> Vec<String> {
        resolve(
            segs,
            &self.module,
            self.self_ty.as_deref(),
            self.crate_name,
            self.imports,
        )
    }

    fn add(&mut self, target: RefTarget, span: Span) {
        self.out.references.push(Reference {
            from: self.from.clone(),
            target,
            line: span.start().line,
        });
    }

    fn note_root(&mut self, raw: &[String]) {
        if let Some(first) = raw.first() {
            let local = matches!(first.as_str(), "crate" | "self" | "super" | "Self")
                || self.imports.contains_key(first);
            if !local && raw.len() >= 2 {
                self.out.external_roots.insert(first.clone());
            }
        }
        if let Some(full) = raw.first().and_then(|f| self.imports.get(f)) {
            if let Some(root) = full.first() {
                if root != self.crate_name {
                    self.out.external_roots.insert(root.clone());
                }
            }
        }
    }

    /// Macro bodies are opaque tokens to `syn`. Parse them as a
    /// comma-separated expression list when possible; otherwise record every
    /// identifier followed by `(` or `::` as a path reference (conservative).
    fn macro_tokens(&mut self, tokens: &TokenStream) {
        let parser = Punctuated::<Expr, Token![,]>::parse_terminated;
        if let Ok(exprs) = syn::parse::Parser::parse2(parser, tokens.clone()) {
            for e in &exprs {
                self.visit_expr(e);
            }
            return;
        }
        let trees: Vec<TokenTree> = tokens.clone().into_iter().collect();
        for (i, tt) in trees.iter().enumerate() {
            match tt {
                TokenTree::Ident(id) => {
                    let next_is_call = matches!(trees.get(i + 1), Some(TokenTree::Group(g)) if g.delimiter() == proc_macro2::Delimiter::Parenthesis);
                    let next_is_path =
                        matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':');
                    if next_is_call || next_is_path {
                        let segs = self.resolved(&[id.to_string()]);
                        self.add(RefTarget::Path(segs), id.span());
                    }
                }
                TokenTree::Group(g) => self.macro_tokens(&g.stream()),
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for RefVisitor<'_, '_> {
    fn visit_path(&mut self, p: &'ast syn::Path) {
        let raw = path_segments(p);
        self.note_root(&raw);
        let segs = self.resolved(&raw);
        let span = p
            .segments
            .first()
            .map(|s| s.ident.span())
            .unwrap_or_else(Span::call_site);
        if !segs.is_empty() {
            self.add(RefTarget::Path(segs), span);
        }
        visit::visit_path(self, p);
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        self.add(RefTarget::Method(m.method.to_string()), m.method.span());
        visit::visit_expr_method_call(self, m);
    }

    fn visit_expr_field(&mut self, f: &'ast syn::ExprField) {
        if let syn::Member::Named(id) = &f.member {
            self.add(RefTarget::Field(id.to_string()), id.span());
        }
        visit::visit_expr_field(self, f);
    }

    fn visit_expr_struct(&mut self, s: &'ast syn::ExprStruct) {
        let segs = self.resolved(&path_segments(&s.path));
        let span = s
            .path
            .segments
            .first()
            .map(|x| x.ident.span())
            .unwrap_or_else(Span::call_site);
        self.add(
            RefTarget::StructLiteral {
                path: segs,
                has_rest: s.rest.is_some(),
            },
            span,
        );
        for field in &s.fields {
            if let syn::Member::Named(id) = &field.member {
                self.add(RefTarget::Field(id.to_string()), id.span());
            }
        }
        visit::visit_expr_struct(self, s);
    }

    fn visit_expr_match(&mut self, m: &'ast syn::ExprMatch) {
        let catch_all = m.arms.iter().any(|a| is_catch_all(&a.pat));
        if !catch_all {
            let mut enums = BTreeSet::new();
            for arm in &m.arms {
                variant_owners(&arm.pat, &mut enums);
            }
            for e in enums {
                let segs = self.resolved(&e);
                self.add(
                    RefTarget::ExhaustiveMatch { enum_path: segs },
                    m.match_token.span,
                );
            }
        }
        visit::visit_expr_match(self, m);
    }

    /// `#[derive(serde::Serialize, Clone)]`: derive lists are opaque tokens
    /// to `syn`; parse them as paths so derive crates count as uses.
    fn visit_attribute(&mut self, a: &'ast syn::Attribute) {
        if a.path().is_ident("derive") {
            let parser = Punctuated::<syn::Path, Token![,]>::parse_terminated;
            if let Ok(paths) = a.parse_args_with(parser) {
                for p in &paths {
                    self.visit_path(p);
                }
            }
        }
        visit::visit_attribute(self, a);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.macro_tokens(&mac.tokens);
        visit::visit_macro(self, mac);
    }

    // Nested items get their own symbols; do not attribute their contents
    // to the enclosing item twice.
    fn visit_item(&mut self, _: &'ast Item) {}
}

/// A pattern that matches anything (`_`, a binding without subpattern).
fn is_catch_all(p: &Pat) -> bool {
    match p {
        Pat::Wild(_) => true,
        Pat::Ident(i) => i.subpat.is_none() && !starts_upper(&i.ident.to_string()),
        Pat::Or(o) => o.cases.iter().any(is_catch_all),
        Pat::Paren(p) => is_catch_all(&p.pat),
        _ => false,
    }
}

fn starts_upper(s: &str) -> bool {
    s.chars().next().is_some_and(char::is_uppercase)
}

/// Enum paths named by variant patterns (`Outcome::Retry(..)` → `Outcome`).
fn variant_owners(p: &Pat, out: &mut BTreeSet<Vec<String>>) {
    let path = match p {
        Pat::TupleStruct(t) => Some(&t.path),
        Pat::Struct(s) => Some(&s.path),
        Pat::Path(p) => Some(&p.path),
        Pat::Or(o) => {
            for c in &o.cases {
                variant_owners(c, out);
            }
            None
        }
        Pat::Reference(r) => {
            variant_owners(&r.pat, out);
            None
        }
        Pat::Paren(p) => {
            variant_owners(&p.pat, out);
            None
        }
        _ => None,
    };
    if let Some(path) = path {
        let segs = path_segments(path);
        if segs.len() >= 2 {
            out.insert(segs[..segs.len() - 1].to_vec());
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn child(parent: &[String], name: &str) -> Vec<String> {
    let mut v = parent.to_vec();
    v.push(name.to_string());
    v
}

fn path_segments(p: &syn::Path) -> Vec<String> {
    p.segments.iter().map(|s| s.ident.to_string()).collect()
}

/// The name of an impl's self type (`Router`, `Router<T>` → `Router`).
fn type_name(t: &Type) -> Option<String> {
    match t {
        Type::Path(tp) => tp.path.segments.last().map(|s| s.ident.to_string()),
        Type::Reference(r) => type_name(&r.elem),
        Type::Paren(p) => type_name(&p.elem),
        Type::Group(g) => type_name(&g.elem),
        _ => None,
    }
}

fn vis_of(v: &Visibility) -> SymbolVisibility {
    match v {
        Visibility::Public(_) => SymbolVisibility::Public,
        Visibility::Restricted(_) => SymbolVisibility::Restricted,
        Visibility::Inherited => SymbolVisibility::Private,
    }
}

/// Non-doc attributes (`#[derive(..)]`, `#[repr(..)]`, `#[non_exhaustive]`)
/// rendered for a container signature: they change what callers may do.
fn attrs_sig(attrs: &[syn::Attribute]) -> String {
    attrs
        .iter()
        .filter(|a| !a.path().is_ident("doc"))
        .map(|a| format!("{} ", tok(a)))
        .collect()
}

/// Normalized token rendering (formatting- and comment-insensitive).
fn tok(t: &dyn ToTokens) -> String {
    t.to_token_stream().to_string()
}

/// Token rendering with `#[doc = ...]` attributes (doc comments) removed,
/// so documentation edits never count as implementation changes.
fn tokens_without_docs(t: &dyn ToTokens) -> String {
    let mut out = String::new();
    strip_docs(t.to_token_stream(), &mut out);
    out
}

fn strip_docs(ts: TokenStream, out: &mut String) {
    let trees: Vec<TokenTree> = ts.into_iter().collect();
    let mut i = 0;
    while i < trees.len() {
        if let (TokenTree::Punct(p), Some(TokenTree::Group(g))) = (&trees[i], trees.get(i + 1)) {
            if p.as_char() == '#' && g.delimiter() == proc_macro2::Delimiter::Bracket {
                let first = g.stream().into_iter().next();
                if matches!(first, Some(TokenTree::Ident(id)) if id == "doc") {
                    i += 2;
                    continue;
                }
            }
        }
        match &trees[i] {
            TokenTree::Group(g) => {
                out.push_str(match g.delimiter() {
                    proc_macro2::Delimiter::Parenthesis => "(",
                    proc_macro2::Delimiter::Brace => "{",
                    proc_macro2::Delimiter::Bracket => "[",
                    proc_macro2::Delimiter::None => "",
                });
                strip_docs(g.stream(), out);
                out.push_str(match g.delimiter() {
                    proc_macro2::Delimiter::Parenthesis => ")",
                    proc_macro2::Delimiter::Brace => "}",
                    proc_macro2::Delimiter::Bracket => "]",
                    proc_macro2::Delimiter::None => "",
                });
            }
            other => {
                out.push_str(&other.to_string());
                out.push(' ');
            }
        }
        i += 1;
    }
}

/// FNV-1a, 64-bit: stable across builds and platforms (unlike std's
/// `DefaultHasher`), so persisted body hashes stay comparable (§3).
pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Span helper for types (positional fields have no ident).
trait SpanStart {
    fn span_start(&self) -> Span;
}

impl SpanStart for Type {
    fn span_start(&self) -> Span {
        self.to_token_stream()
            .into_iter()
            .next()
            .map(|t| t.span())
            .unwrap_or_else(Span::call_site)
    }
}
