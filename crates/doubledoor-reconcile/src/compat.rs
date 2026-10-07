//! The §52 compatibility report: does the session's retained code still
//! compile against the canonical changes merged around it?
//!
//! Compiler diagnostics (file, line) from validating the candidate are
//! mapped onto the units of the candidate's files. Every unit the session
//! contributed — contested units it kept and uncontested units it changed —
//! is reported `compatible`, `incompatible` (with the diagnostics inside
//! it), or `unverified` when validation never reached the compiler.

use serde::{Deserialize, Serialize};

use doubledoor_git::Git;
use doubledoor_surface::semantic::crate_map_at;

use crate::candidate::Candidate;
use crate::merge::Side;
use crate::units::file_units;

/// A compiler diagnostic located in the candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Located {
    pub file: String,
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatResult {
    pub surface: String,
    pub file: String,
    /// `compatible`, `incompatible`, or `unverified`.
    pub status: String,
    pub diagnostics: Vec<String>,
}

/// `compiled` is false when no compiler gate ran to completion.
pub fn compatibility(
    git: &Git,
    c: &Candidate,
    diags: &[Located],
    compiled: bool,
) -> Vec<CompatResult> {
    let map = crate_map_at(git, &c.commit).unwrap_or_default();
    let mut out = Vec::new();
    for f in &c.files {
        if !f.path.ends_with(".rs") || f.outcome == "canonical" || f.outcome == "deleted" {
            continue;
        }
        let kept_session: Vec<&str> = f
            .contested
            .iter()
            .filter(|x| x.kept == Side::Session)
            .map(|x| x.surface.as_str())
            .collect();
        let Some(module) = map.module_of(&f.path) else {
            continue;
        };
        let Ok(bytes) = git.run_git_bytes(&["show", &format!("{}:{}", c.commit, f.path)]) else {
            continue;
        };
        let src = String::from_utf8_lossy(&bytes).into_owned();
        let Ok(units) = file_units(&src, &module) else {
            continue;
        };
        let line_of = |byte: usize| src[..byte].matches('\n').count() + 1;
        let mut flat = Vec::new();
        for u in &units.units {
            if u.children.is_empty() {
                flat.push(u.clone());
            } else {
                flat.extend(u.children.iter().cloned());
            }
        }
        for u in flat {
            let session_owned = f.outcome == "session"
                || kept_session.contains(&u.surface.as_str())
                || f.from_session.contains(&u.surface);
            if !session_owned || !u.surface.starts_with("rust:") {
                continue;
            }
            let (from, to) = (line_of(u.start), line_of(u.end));
            let inside: Vec<String> = diags
                .iter()
                .filter(|d| d.file == f.path && d.line >= from && d.line <= to)
                .map(|d| format!("{}:{}: {}", d.file, d.line, d.message))
                .collect();
            out.push(CompatResult {
                surface: u.surface.clone(),
                file: f.path.clone(),
                status: if !compiled {
                    "unverified"
                } else if inside.is_empty() {
                    "compatible"
                } else {
                    "incompatible"
                }
                .to_string(),
                diagnostics: inside,
            });
        }
    }
    out
}
