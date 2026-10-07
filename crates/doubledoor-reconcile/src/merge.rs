//! Three-way merge of one file between the session's base (B), canonical
//! (H), and the session's speculative snapshot (S) — §16 steps 5–8.
//!
//! Rust files merge at unit granularity ([`crate::units`]): a unit changed
//! by one side takes that side's text; a unit both sides changed differently
//! is **contested** and is never blended — it keeps the session's version in
//! [`Mode::Speculative`] (the §52 `E'`: "Beta's speculative work … − Alpha's
//! dispatch implementation") or canonical's in [`Mode::Converge`] (so the
//! session's independent work can converge onto canonical while the
//! contested unit stays isolated in its workspace). Other files, and Rust
//! files that do not parse, merge textually with `git merge-file`; a
//! textual conflict makes the whole file contested.

use serde::{Deserialize, Serialize};

use crate::units::{file_units, FileUnits, Unit};

/// How contested surfaces are resolved in a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// Contested surfaces keep the session's version: the candidate is the
    /// session's world with every independent canonical change imported.
    Speculative,
    /// Contested surfaces keep canonical's version: the candidate carries
    /// only the session's uncontested work, so it can be promoted while the
    /// contested work stays isolated in the session.
    Converge,
}

/// Which side a contested unit's text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Session,
    Canonical,
}

/// One contested unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contested {
    pub key: String,
    /// `rust:…`, `item:…`, or `file:<path>` for whole-file contests.
    pub surface: String,
    pub kept: Side,
}

/// How a file was merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Method {
    /// Only one side changed the file (or both made the same change).
    OneSided,
    /// Rust unit-level three-way merge.
    Units,
    /// `git merge-file` three-way merge.
    Textual,
}

/// The merged file (`None` = deleted) and what was contested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMerge {
    pub content: Option<String>,
    pub method: Method,
    pub contested: Vec<Contested>,
    /// Units taken from the session's side without contest.
    pub from_session: Vec<String>,
    /// Units taken from canonical's side without contest.
    pub from_canonical: Vec<String>,
}

/// Why a unit-level merge could not be performed (the caller falls back to
/// a textual merge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fallback(pub String);

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Unit-level merge of three present versions of a Rust file.
pub fn merge_rust(
    base: &str,
    canonical: &str,
    session: &str,
    module: &[String],
    mode: Mode,
) -> Result<FileMerge, Fallback> {
    merge_rust_selected(base, canonical, session, module, mode, None)
}

/// A predicate over unit surfaces (`rust:…`, `item:…`).
pub type Select<'a> = &'a dyn Fn(&str) -> bool;

/// [`merge_rust`] taking only the session-side changes whose unit surface
/// satisfies `select` (all of them when `None`); unselected session changes
/// are ignored as if the session had not made them. This is selective
/// import (§15): `merge_rust_selected(P, P, G, …, Some(chosen))` is the
/// parent `P` of a ghost commit `G` plus only the chosen units of `G`.
pub fn merge_rust_selected(
    base: &str,
    canonical: &str,
    session: &str,
    module: &[String],
    mode: Mode,
    select: Option<Select<'_>>,
) -> Result<FileMerge, Fallback> {
    let parse = |s: &str, side: &str| {
        file_units(s, module).map_err(|e| Fallback(format!("{side} version: {e}")))
    };
    let (b, h, s) = (
        parse(base, "base")?,
        parse(canonical, "canonical")?,
        parse(session, "session")?,
    );
    let mut out = FileMerge {
        content: None,
        method: Method::Units,
        contested: Vec::new(),
        from_session: Vec::new(),
        from_canonical: Vec::new(),
    };
    let mut edits: Vec<Edit> = Vec::new();

    // Preamble (inner attributes, module docs).
    let (pb, ph, ps) = (
        &base[..b.preamble_end],
        &canonical[..h.preamble_end],
        &session[..s.preamble_end],
    );
    let preamble_session = if select.is_some_and(|f| !f("item:preamble")) {
        pb
    } else {
        ps
    };
    three_way(
        "preamble",
        "item:preamble",
        Some(pb),
        Some(ph),
        Some(preamble_session),
        mode,
        &mut out,
        |take| {
            edits.push(Edit {
                start: 0,
                end: h.preamble_end,
                text: take.unwrap_or_default().to_string(),
            })
        },
    );

    let keys = ordered_keys(&h, &s, &b);
    for key in &keys {
        let (ub, uh, us) = (b.get(key), h.get(key), s.get(key));
        let tb = ub.map(|u| u.text(base));
        let th = uh.map(|u| u.text(canonical));
        let mut ts = us.map(|u| u.text(session));
        let surface = uh
            .or(us)
            .or(ub)
            .map(|u| u.surface.clone())
            .unwrap_or_default();

        // Impls changed on both sides — or under a selection, which may pick
        // single members — merge member by member if the shells agree.
        if let (Some(ub), Some(uh), Some(us)) = (ub, uh, us) {
            if ts != tb
                && (select.is_some() || (th != tb && ts != th))
                && ub.shell(base) == uh.shell(canonical)
                && ub.shell(base) == us.shell(session)
                && uh.body_end.is_some()
            {
                merge_members(
                    ub, uh, us, base, canonical, session, mode, select, &mut out, &mut edits,
                );
                continue;
            }
        }
        // Unselected session changes are ignored.
        if let Some(f) = select {
            let chosen =
                f(&surface) || us.is_some_and(|u| u.children.iter().any(|c| f(&c.surface)));
            if !chosen {
                ts = tb;
            }
        }

        let anchor = insertion_point(key, &s, &h, canonical);
        three_way(key, &surface, tb, th, ts, mode, &mut out, |take| {
            match (uh, take) {
                (Some(u), Some(text)) => edits.push(Edit {
                    start: u.start,
                    end: u.end,
                    text: text.to_string(),
                }),
                (Some(u), None) => edits.push(Edit {
                    start: u.start,
                    end: trailing_newline(canonical, u.end),
                    text: String::new(),
                }),
                (None, Some(text)) => edits.push(Edit {
                    start: anchor,
                    end: anchor,
                    text: if anchor == h.preamble_end {
                        format!("{text}\n\n")
                    } else {
                        format!("\n\n{text}")
                    },
                }),
                (None, None) => {}
            }
        });
    }

    let merged = apply(canonical, edits);
    // The merged file must itself parse; otherwise the unit merge produced
    // something we cannot vouch for.
    file_units(&merged, module)
        .map_err(|e| Fallback(format!("unit-merged result does not parse: {e}")))?;
    out.content = Some(merged);
    Ok(out)
}

/// The three-way rule for one unit. `take(Some(text))` replaces/inserts,
/// `take(None)` deletes; not calling it keeps canonical's version.
#[allow(clippy::too_many_arguments)]
fn three_way(
    key: &str,
    surface: &str,
    b: Option<&str>,
    h: Option<&str>,
    s: Option<&str>,
    mode: Mode,
    out: &mut FileMerge,
    mut take: impl FnMut(Option<&str>),
) {
    let s_changed = s != b;
    let h_changed = h != b;
    if !s_changed || s == h {
        if h_changed {
            out.from_canonical.push(surface.to_string());
        }
        return;
    }
    if !h_changed {
        out.from_session.push(surface.to_string());
        take(s);
        return;
    }
    let kept = match mode {
        Mode::Speculative => {
            take(s);
            Side::Session
        }
        Mode::Converge => Side::Canonical,
    };
    out.contested.push(Contested {
        key: key.to_string(),
        surface: surface.to_string(),
        kept,
    });
}

#[allow(clippy::too_many_arguments)]
fn merge_members(
    ub: &Unit,
    uh: &Unit,
    us: &Unit,
    base: &str,
    canonical: &str,
    session: &str,
    mode: Mode,
    select: Option<Select<'_>>,
    out: &mut FileMerge,
    edits: &mut Vec<Edit>,
) {
    let keys = {
        let mut k: Vec<String> = uh.children.iter().map(|c| c.key.clone()).collect();
        for c in us.children.iter().chain(ub.children.iter()) {
            if !k.contains(&c.key) {
                k.push(c.key.clone());
            }
        }
        k
    };
    let body_end = uh.body_end.expect("impl units carry their closing brace");
    for key in &keys {
        let find = |u: &Unit| u.children.iter().find(|c| &c.key == key).cloned();
        let (cb, ch, cs) = (find(ub), find(uh), find(us));
        let surface = ch
            .as_ref()
            .or(cs.as_ref())
            .or(cb.as_ref())
            .map(|c| c.surface.clone())
            .unwrap_or_default();
        let tb = cb.as_ref().map(|c| c.text(base));
        let th = ch.as_ref().map(|c| c.text(canonical));
        let mut ts = cs.as_ref().map(|c| c.text(session));
        if select.is_some_and(|f| !f(&surface)) {
            ts = tb;
        }
        three_way(key, &surface, tb, th, ts, mode, out, |take| {
            match (&ch, take) {
                (Some(c), Some(text)) => edits.push(Edit {
                    start: c.start,
                    end: c.end,
                    text: text.to_string(),
                }),
                (Some(c), None) => edits.push(Edit {
                    start: c.start,
                    end: trailing_newline(canonical, c.end),
                    text: String::new(),
                }),
                (None, Some(text)) => edits.push(Edit {
                    start: body_end,
                    end: body_end,
                    text: format!("\n    {text}\n"),
                }),
                (None, None) => {}
            }
        });
    }
}

/// Keys in canonical's order, then session-only keys in the session's
/// order, then base-only keys.
fn ordered_keys(h: &FileUnits, s: &FileUnits, b: &FileUnits) -> Vec<String> {
    let mut keys: Vec<String> = h.units.iter().map(|u| u.key.clone()).collect();
    for u in s.units.iter().chain(b.units.iter()) {
        if !keys.contains(&u.key) {
            keys.push(u.key.clone());
        }
    }
    keys
}

/// Where a session-only unit goes in canonical's file: after the nearest
/// preceding unit (in the session's order) that canonical also has, else
/// right after the preamble.
fn insertion_point(key: &str, s: &FileUnits, h: &FileUnits, _canonical: &str) -> usize {
    let Some(pos) = s.units.iter().position(|u| u.key == key) else {
        return h.preamble_end;
    };
    s.units[..pos]
        .iter()
        .rev()
        .find_map(|prev| h.get(&prev.key).map(|u| u.end))
        .unwrap_or(h.preamble_end)
}

fn trailing_newline(src: &str, end: usize) -> usize {
    if src[end..].starts_with('\n') {
        end + 1
    } else {
        end
    }
}

fn apply(src: &str, mut edits: Vec<Edit>) -> String {
    // Stable order: by start, insertions (empty spans) after replacements
    // that end there, so several insertions at one anchor keep their order.
    edits.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    let mut out = String::with_capacity(src.len());
    let mut at = 0;
    for e in edits {
        if e.start < at {
            continue; // overlapping edit: cannot happen for disjoint units
        }
        out.push_str(&src[at..e.start]);
        out.push_str(&e.text);
        at = e.end;
    }
    out.push_str(&src[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Vec<String> {
        vec!["app".into(), "router".into()]
    }

    const BASE: &str = r#"use crate::config::RouterConfig;

pub fn dispatch(cfg: &RouterConfig) -> Result<u8, ()> {
    Ok(cfg.timeout)
}

pub fn log_route(r: &str) -> usize {
    r.len()
}

impl Router {
    pub fn new() -> Self {
        Router
    }
}
"#;

    #[test]
    fn independent_units_converge_and_contested_units_are_not_blended() {
        // Canonical: dispatch signature change + log_route body change +
        // a new method.
        let canonical = BASE
            .replace("-> Result<u8, ()>", "-> Result<u8, String>")
            .replace("    r.len()", "    r.len() + 1")
            .replace(
                "        Router\n    }\n}",
                "        Router\n    }\n\n    pub fn reset(&mut self) {}\n}",
            );
        // Session: reworks dispatch's body + adds a new fn.
        let session = BASE
            .replace(
                "    Ok(cfg.timeout)",
                "    // beta\n    Ok(cfg.timeout * 2)",
            )
            .replace(
                "pub fn log_route",
                "pub fn trace(r: &str) {\n    let _ = r;\n}\n\npub fn log_route",
            );
        for mode in [Mode::Speculative, Mode::Converge] {
            let r = merge_rust(BASE, &canonical, &session, &m(), mode).unwrap();
            let text = r.content.unwrap();
            // Independent portions from both sides converge.
            assert!(text.contains("r.len() + 1"), "{text}");
            assert!(text.contains("pub fn reset"), "{text}");
            assert!(text.contains("pub fn trace"), "{text}");
            // dispatch was changed by both: contested, never blended.
            assert_eq!(r.contested.len(), 1);
            assert_eq!(r.contested[0].surface, "rust:app::router::dispatch");
            let blended = text.contains("Result<u8, String>") && text.contains("cfg.timeout * 2");
            assert!(!blended, "contested units must not mix both sides:\n{text}");
            match mode {
                Mode::Speculative => {
                    assert!(text.contains("cfg.timeout * 2"));
                    assert_eq!(r.contested[0].kept, Side::Session);
                }
                Mode::Converge => {
                    assert!(text.contains("Result<u8, String>"));
                    assert_eq!(r.contested[0].kept, Side::Canonical);
                }
            }
        }
    }

    #[test]
    fn member_level_merge_inside_a_shared_impl() {
        let canonical = BASE.replace(
            "        Router\n    }\n}",
            "        Router\n    }\n\n    pub fn a(&self) {}\n}",
        );
        let session = BASE.replace(
            "        Router\n    }\n}",
            "        Router\n    }\n\n    pub fn b(&self) {}\n}",
        );
        let r = merge_rust(BASE, &canonical, &session, &m(), Mode::Speculative).unwrap();
        let text = r.content.unwrap();
        assert!(
            text.contains("pub fn a(&self)") && text.contains("pub fn b(&self)"),
            "{text}"
        );
        assert!(r.contested.is_empty());
        assert!(r
            .from_session
            .contains(&"rust:app::router::Router::b".to_string()));
    }

    #[test]
    fn deletions_and_identical_changes() {
        let both = BASE.replace("    r.len()", "    r.len() * 2");
        let canonical = both.clone();
        let session = both.replace(
            "impl Router {\n    pub fn new() -> Self {\n        Router\n    }\n}\n",
            "",
        );
        let r = merge_rust(BASE, &canonical, &session, &m(), Mode::Speculative).unwrap();
        let text = r.content.unwrap();
        assert!(text.contains("r.len() * 2"));
        assert!(
            !text.contains("impl Router"),
            "session's deletion applies: {text}"
        );
        assert!(r.contested.is_empty());
    }

    #[test]
    fn selection_takes_only_the_chosen_units() {
        let ghost = BASE
            .replace("    r.len()", "    r.len() + 1")
            .replace("-> Result<u8, ()>", "-> Result<u8, String>")
            .replace(
                "        Router\n    }\n}",
                "        Router\n    }\n\n    pub fn reset(&mut self) {}\n}",
            );
        let pick =
            |s: &str| s == "rust:app::router::log_route" || s == "rust:app::router::Router::reset";
        let r =
            merge_rust_selected(BASE, BASE, &ghost, &m(), Mode::Speculative, Some(&pick)).unwrap();
        let text = r.content.unwrap();
        assert!(text.contains("r.len() + 1"));
        assert!(text.contains("pub fn reset"));
        assert!(
            text.contains("Result<u8, ()>"),
            "unselected dispatch keeps the base version"
        );
    }

    #[test]
    fn unparseable_sides_fall_back() {
        assert!(merge_rust(BASE, BASE, "fn (", &m(), Mode::Speculative).is_err());
    }
}
