//! Dream VPS coordination (§51, `dream-vps` integration): what a remote node
//! can do, from the Dream VPS node agent's own capability report.
//!
//! `dreamseq-node capabilities` prints the node's `NodeCapabilities`
//! (`architecture`, `vcpus`, `memory_mb`, `os`, `ssh`, `git`, `ollama`,
//! `harnesses`). The agent does not list Doubledoor among its harnesses, so
//! Doubledoor's presence is established separately (`doubledoor --version`
//! over the same transport).

use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::config::Remote;

/// The Dream VPS `NodeCapabilities` report (as printed by `dreamseq-node`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapabilities {
    pub architecture: String,
    pub vcpus: u32,
    pub memory_mb: u64,
    #[serde(default)]
    pub os: String,
    pub ssh: bool,
    pub git: bool,
    #[serde(default)]
    pub ollama: bool,
    #[serde(default)]
    pub harnesses: Vec<String>,
}

/// What Doubledoor records about a probed node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFacts {
    pub capabilities: NodeCapabilities,
    /// `doubledoor --version` on the node, if it answered.
    pub doubledoor_version: Option<String>,
    pub agent: String,
    pub probed_at_millis: u64,
}

fn run(remote: &Remote, args: &[&str]) -> Result<String, String> {
    let argv = remote.command(args)?;
    let (prog, rest) = argv.split_first().ok_or("empty command")?;
    let out = Command::new(prog)
        .args(rest)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`{}` exited {:?}: {}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Probe a node through its transport. `agent` is the node agent binary
/// (default `dreamseq-node`).
pub fn probe(remote: &Remote, agent: &str, now_millis: u64) -> Result<NodeFacts, String> {
    let raw = run(remote, &[agent, "capabilities"])?;
    let capabilities: NodeCapabilities = serde_json::from_str(&raw)
        .map_err(|e| format!("node agent output is not a NodeCapabilities report: {e}"))?;
    let doubledoor_version = run(remote, &[&remote.doubledoor, "--version"]).ok();
    Ok(NodeFacts {
        capabilities,
        doubledoor_version,
        agent: agent.to_string(),
        probed_at_millis: now_millis,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_real_dreamseq_node_report() {
        // Captured from `dreamseq-node capabilities` on this machine.
        let raw = r#"{"architecture":"x86_64","vcpus":12,"memory_mb":7543,"os":"linux","ssh":true,"git":true,"ollama":true,"harnesses":["kaptaind","uni","fract","traci","ollama"]}"#;
        let c: NodeCapabilities = serde_json::from_str(raw).unwrap();
        assert_eq!(c.vcpus, 12);
        assert!(c.git && c.ssh);
        assert!(c.harnesses.contains(&"fract".to_string()));
        assert!(serde_json::from_str::<NodeCapabilities>(r#"{"vcpus": "lots"}"#).is_err());
    }
}
