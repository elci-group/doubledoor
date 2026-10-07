//! Placement of remote sessions on Dream VPS nodes (§51 item 2).
//!
//! `--remote <name>` names a remote directly. `--remote auto` chooses among
//! remotes that have been probed (`doubledoor remote probe`): each is
//! connected to and must be recovery-clean (§51), in sync with local
//! canonical, report `git`, have Doubledoor installed, and offer every
//! `--require-harness`. Among those, the node with the least load per vCPU
//! wins (load = sessions this repository has opened there), then more
//! vCPUs, then more memory, then name — a deterministic choice. Every
//! excluded remote is reported with its reason.

use serde::Serialize;

use doubledoor_remote::node::NodeFacts;
use doubledoor_remote::RemotesConfig;

use crate::error::CliError;

#[derive(Debug, Serialize)]
pub struct Placement {
    pub chosen: String,
    pub reason: String,
    pub excluded: Vec<(String, String)>,
}

pub fn resolve(
    git: &doubledoor_git::Git,
    cfg: &RemotesConfig,
    name: &str,
    require_harness: &[String],
) -> Result<Placement, CliError> {
    if name != "auto" {
        if !cfg.remotes.contains_key(name) {
            return Err(CliError::NotFound(format!("remote `{name}`")));
        }
        return Ok(Placement {
            chosen: name.to_string(),
            reason: "named explicitly".to_string(),
            excluded: Vec::new(),
        });
    }
    let local = git.head_oid()?;
    let links = std::fs::read_dir(crate::commands::store_root(git).join("remote-sessions"))
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .filter_map(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut excluded = Vec::new();
    let mut eligible: Vec<(String, NodeFacts, usize)> = Vec::new();
    for (rname, r) in &cfg.remotes {
        let Some(facts) = r
            .node
            .clone()
            .and_then(|n| serde_json::from_value::<NodeFacts>(n).ok())
        else {
            excluded.push((
                rname.clone(),
                "never probed (`doubledoor remote probe`)".to_string(),
            ));
            continue;
        };
        let caps = &facts.capabilities;
        let why = if !caps.git {
            Some("node reports no git".to_string())
        } else if facts.doubledoor_version.is_none() {
            Some("Doubledoor is not installed on the node".to_string())
        } else {
            require_harness
                .iter()
                .find(|h| !caps.harnesses.contains(h))
                .map(|h| format!("node lacks harness `{h}`"))
        };
        if let Some(why) = why {
            excluded.push((rname.clone(), why));
            continue;
        }
        match crate::remote::connect_ready(r, &["sessions"]) {
            Err(e) => {
                excluded.push((rname.clone(), e.to_string()));
                continue;
            }
            Ok(mut c) => {
                let head = c
                    .call("status", serde_json::json!({}))
                    .ok()
                    .and_then(|s| s["canonical_head"].as_str().map(ToString::to_string))
                    .unwrap_or_default();
                if head != local.as_str() {
                    excluded.push((rname.clone(), format!("canonical out of sync ({head})")));
                    continue;
                }
            }
        }
        let load = links
            .iter()
            .filter(|l| l["remote"] == rname.as_str())
            .count();
        eligible.push((rname.clone(), facts, load));
    }
    eligible.sort_by(|(an, af, al), (bn, bf, bl)| {
        let (av, bv) = (
            af.capabilities.vcpus.max(1) as u64,
            bf.capabilities.vcpus.max(1) as u64,
        );
        // al/av < bl/bv  ⇔  al*bv < bl*av
        ((*al as u64) * bv)
            .cmp(&((*bl as u64) * av))
            .then(bv.cmp(&av))
            .then(bf.capabilities.memory_mb.cmp(&af.capabilities.memory_mb))
            .then(an.cmp(bn))
    });
    match eligible.first() {
        Some((n, f, load)) => Ok(Placement {
            chosen: n.clone(),
            reason: format!(
                "{load} session(s) on {} vCPU(s), {} MB; {} eligible node(s)",
                f.capabilities.vcpus,
                f.capabilities.memory_mb,
                eligible.len()
            ),
            excluded,
        }),
        None => Err(CliError::InvalidTransition(format!(
            "no Dream VPS remote can host the session: {}",
            excluded
                .iter()
                .map(|(n, w)| format!("{n}: {w}"))
                .collect::<Vec<_>>()
                .join("; ")
        ))),
    }
}
