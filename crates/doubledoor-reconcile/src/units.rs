//! Merge units of a Rust source file: every top-level item, and every item
//! inside an `impl` block, with its exact byte span (outer attributes and doc
//! comments included) and a key that identifies it across versions.
//!
//! Units are what reconciliation merges and what selective import moves:
//! a unit changed by one side takes that side's text; a unit changed by both
//! sides is contested and is never blended (§16 steps 5–8).

use proc_macro2::LineColumn;
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::{ImplItem, Item};

use doubledoor_surface::rust::{with_parsed_file, RustParseError};

/// One mergeable unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// Identity within the file, stable across versions (`fn dispatch`,
    /// `struct RouterConfig`, `impl Router`, `use crate :: x`).
    pub key: String,
    /// The surface this unit represents: `rust:<path>` for named items and
    /// impl members, `item:<key>` otherwise.
    pub surface: String,
    /// Byte span in the source.
    pub start: usize,
    pub end: usize,
    /// Impl blocks: their members, and the byte offset of the closing brace.
    pub children: Vec<Unit>,
    pub body_end: Option<usize>,
}

impl Unit {
    pub fn text<'s>(&self, src: &'s str) -> &'s str {
        &src[self.start..self.end]
    }

    /// For impl blocks: the text with every member removed (header, where
    /// clause, attributes). Two versions of an impl with equal shells differ
    /// only in their members.
    pub fn shell(&self, src: &str) -> String {
        let mut out = String::new();
        let mut at = self.start;
        for c in &self.children {
            out.push_str(&src[at..c.start]);
            at = c.end;
        }
        out.push_str(&src[at..self.end]);
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// The units of one file, plus where its preamble (inner attributes, module
/// docs, leading comments) ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUnits {
    pub preamble_end: usize,
    pub units: Vec<Unit>,
}

impl FileUnits {
    pub fn get(&self, key: &str) -> Option<&Unit> {
        self.units.iter().find(|u| u.key == key)
    }
}

/// Split `src` (the file at module path `module`) into units.
pub fn file_units(src: &str, module: &[String]) -> Result<FileUnits, RustParseError> {
    let owned = src.to_string();
    let module = module.to_vec();
    with_parsed_file(src, move |file| {
        let lines = LineIndex::new(&owned);
        let mut units = Vec::new();
        let mut seen = std::collections::BTreeMap::<String, usize>::new();
        for item in &file.items {
            let (start, end) = lines.span(item.span());
            let base_key = item_key(item);
            let n = seen.entry(base_key.clone()).or_insert(0);
            *n += 1;
            let key = if *n == 1 {
                base_key
            } else {
                format!("{base_key}#{n}")
            };
            let surface = item_surface(item, &module).unwrap_or_else(|| format!("item:{key}"));
            let (children, body_end) = match item {
                Item::Impl(imp) => {
                    let owner = impl_owner(imp);
                    let mut kids = Vec::new();
                    let mut kseen = std::collections::BTreeMap::<String, usize>::new();
                    for ii in &imp.items {
                        let (s, e) = lines.span(ii.span());
                        let k = impl_item_key(ii);
                        let n = kseen.entry(k.clone()).or_insert(0);
                        *n += 1;
                        let k = if *n == 1 { k } else { format!("{k}#{n}") };
                        let name = impl_item_name(ii);
                        kids.push(Unit {
                            surface: match (&owner, name) {
                                (Some(o), Some(nm)) => {
                                    format!("rust:{}::{o}::{nm}", module.join("::"))
                                }
                                _ => format!("item:{key}::{k}"),
                            },
                            key: k,
                            start: s,
                            end: e,
                            children: Vec::new(),
                            body_end: None,
                        });
                    }
                    let close = lines.offset(imp.brace_token.span.close().start());
                    (kids, Some(close))
                }
                _ => (Vec::new(), None),
            };
            units.push(Unit {
                key,
                surface,
                start,
                end,
                children,
                body_end,
            });
        }
        let preamble_end = units.first().map_or(owned.len(), |u| u.start);
        FileUnits {
            preamble_end,
            units,
        }
    })
}

fn tok(t: &dyn ToTokens) -> String {
    t.to_token_stream().to_string()
}

fn item_key(item: &Item) -> String {
    match item {
        Item::Fn(f) => format!("fn {}", f.sig.ident),
        Item::Struct(s) => format!("struct {}", s.ident),
        Item::Enum(e) => format!("enum {}", e.ident),
        Item::Union(u) => format!("union {}", u.ident),
        Item::Trait(t) => format!("trait {}", t.ident),
        Item::TraitAlias(t) => format!("trait {}", t.ident),
        Item::Type(t) => format!("type {}", t.ident),
        Item::Const(c) => format!("const {}", c.ident),
        Item::Static(s) => format!("static {}", s.ident),
        Item::Mod(m) => format!("mod {}", m.ident),
        Item::ExternCrate(e) => format!("extern crate {}", e.ident),
        Item::Use(u) => format!("use {}", tok(&u.tree)),
        Item::Macro(m) => match &m.ident {
            Some(id) => format!("macro_rules {id}"),
            None => format!("macro {}", tok(&m.mac.path)),
        },
        Item::Impl(imp) => {
            let tr = imp
                .trait_
                .as_ref()
                .map(|(bang, p, _)| {
                    format!("{}{} for ", if bang.is_some() { "!" } else { "" }, tok(p))
                })
                .unwrap_or_default();
            format!("impl {} {tr}{}", tok(&imp.generics), tok(&imp.self_ty))
        }
        other => {
            let t = tok(other);
            format!("item {}", t.chars().take(60).collect::<String>())
        }
    }
}

fn item_surface(item: &Item, module: &[String]) -> Option<String> {
    let name = match item {
        Item::Fn(f) => f.sig.ident.to_string(),
        Item::Struct(s) => s.ident.to_string(),
        Item::Enum(e) => e.ident.to_string(),
        Item::Union(u) => u.ident.to_string(),
        Item::Trait(t) => t.ident.to_string(),
        Item::Type(t) => t.ident.to_string(),
        Item::Const(c) => c.ident.to_string(),
        Item::Static(s) => s.ident.to_string(),
        Item::Mod(m) => m.ident.to_string(),
        Item::Macro(m) => m.ident.as_ref()?.to_string(),
        _ => return None,
    };
    Some(format!("rust:{}::{name}", module.join("::")))
}

/// Matches `doubledoor-surface`'s symbol ids: `Type` or `<Type as Trait>`.
fn impl_owner(imp: &syn::ItemImpl) -> Option<String> {
    let self_name = match &*imp.self_ty {
        syn::Type::Path(tp) => tp.path.segments.last()?.ident.to_string(),
        _ => return None,
    };
    Some(match &imp.trait_ {
        None => self_name,
        Some((_, p, _)) => format!("<{self_name} as {}>", p.segments.last()?.ident),
    })
}

fn impl_item_key(ii: &ImplItem) -> String {
    match ii {
        ImplItem::Fn(f) => format!("fn {}", f.sig.ident),
        ImplItem::Const(c) => format!("const {}", c.ident),
        ImplItem::Type(t) => format!("type {}", t.ident),
        other => format!("item {}", tok(other).chars().take(60).collect::<String>()),
    }
}

fn impl_item_name(ii: &ImplItem) -> Option<String> {
    match ii {
        ImplItem::Fn(f) => Some(f.sig.ident.to_string()),
        ImplItem::Const(c) => Some(c.ident.to_string()),
        ImplItem::Type(t) => Some(t.ident.to_string()),
        _ => None,
    }
}

/// Line/column (1-based line, 0-based char column) → byte offsets.
struct LineIndex<'s> {
    src: &'s str,
    starts: Vec<usize>,
}

impl<'s> LineIndex<'s> {
    fn new(src: &'s str) -> Self {
        let mut starts = vec![0];
        starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { src, starts }
    }

    fn offset(&self, lc: LineColumn) -> usize {
        let Some(&line_start) = self.starts.get(lc.line.saturating_sub(1)) else {
            return self.src.len();
        };
        let line = &self.src[line_start..];
        line_start
            + line
                .char_indices()
                .nth(lc.column)
                .map_or(line.len(), |(i, _)| i)
    }

    fn span(&self, span: proc_macro2::Span) -> (usize, usize) {
        (self.offset(span.start()), self.offset(span.end()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"//! Router.
use crate::config::RouterConfig;

/// Dispatch.
pub fn dispatch(x: u8) -> u8 {
    x
}

pub struct Router;

impl Router {
    pub fn new() -> Self {
        Router
    }

    fn héllo(&self) {}
}

impl Default for Router {
    fn default() -> Self {
        Router
    }
}
"#;

    fn m() -> Vec<String> {
        vec!["app".into(), "router".into()]
    }

    #[test]
    fn units_have_exact_spans_and_stable_keys() {
        let fu = file_units(SRC, &m()).unwrap();
        let keys: Vec<&str> = fu.units.iter().map(|u| u.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "use crate :: config :: RouterConfig",
                "fn dispatch",
                "struct Router",
                "impl  Router",
                "impl  Default for Router",
            ]
        );
        let d = fu.get("fn dispatch").unwrap();
        assert_eq!(
            d.text(SRC),
            "/// Dispatch.\npub fn dispatch(x: u8) -> u8 {\n    x\n}"
        );
        assert_eq!(d.surface, "rust:app::router::dispatch");
        assert!(SRC[..fu.preamble_end].starts_with("//! Router."));

        let imp = fu.get("impl  Router").unwrap();
        assert_eq!(imp.children.len(), 2);
        assert_eq!(
            imp.children[1].text(SRC),
            "fn héllo(&self) {}",
            "char columns map to bytes"
        );
        assert_eq!(imp.children[0].surface, "rust:app::router::Router::new");
        assert_eq!(&SRC[imp.body_end.unwrap()..imp.end], "}");
        let tr = fu.get("impl  Default for Router").unwrap();
        assert_eq!(
            tr.children[0].surface,
            "rust:app::router::<Router as Default>::default"
        );
        assert_eq!(imp.shell(SRC), "impl Router { }");
    }

    #[test]
    fn duplicate_keys_are_numbered() {
        let fu = file_units("macro_a!();\nmacro_a!();\n", &m()).unwrap();
        assert_eq!(fu.units[0].key, "macro macro_a");
        assert_eq!(fu.units[1].key, "macro macro_a#2");
    }
}
