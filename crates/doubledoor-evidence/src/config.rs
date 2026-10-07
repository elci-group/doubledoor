//! `.doubledoor/evidence.json`: which providers run and how. Every field is
//! optional; the defaults run every installed local provider in its
//! deterministic/offline mode and publish nowhere.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use doubledoor_core::evidence::{EvidenceKind, EvidenceTier};

fn yes() -> bool {
    true
}

fn default_timeout() -> u64 {
    120
}

/// Settings shared by the built-in adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Binary to run (default: the tool's name, looked up on `PATH`).
    #[serde(default)]
    pub bin: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl Default for ToolConfig {
    fn default() -> Self {
        ToolConfig {
            enabled: true,
            bin: None,
            timeout_secs: default_timeout(),
        }
    }
}

/// ZOC: `dry-run` (local lexical heuristic, no network) or `model` (LLM
/// scoring; needs `GROQ_API_KEY`, and is tagged model-derived).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZocConfig {
    #[serde(flatten)]
    pub tool: ToolConfig,
    #[serde(default = "default_zoc_mode")]
    pub mode: String,
}

fn default_zoc_mode() -> String {
    "dry-run".to_string()
}

impl Default for ZocConfig {
    fn default() -> Self {
        ZocConfig {
            tool: ToolConfig::default(),
            mode: default_zoc_mode(),
        }
    }
}

/// Padagonia: an optional evidence *sink* (it stores the evidence graph
/// with provenance; it does not judge). Off unless configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PadagoniaConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `http://host:port` (plain HTTP to a local/trusted server only).
    #[serde(default = "default_padagonia_url")]
    pub url: String,
    /// Environment variable holding the API key (never stored in config).
    #[serde(default = "default_padagonia_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
}

fn default_padagonia_url() -> String {
    "http://127.0.0.1:7373".to_string()
}

fn default_padagonia_key_env() -> String {
    "PADAGONIA_API_KEY".to_string()
}

fn default_namespace() -> String {
    "default".to_string()
}

impl Default for PadagoniaConfig {
    fn default() -> Self {
        PadagoniaConfig {
            enabled: false,
            url: default_padagonia_url(),
            api_key_env: default_padagonia_key_env(),
            namespace: default_namespace(),
        }
    }
}

/// A provider speaking the generic `doubledoor.evidence/v1` protocol (see
/// [`crate::providers::external`]). Sore plugs in this way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalProvider {
    pub name: String,
    pub argv: Vec<String>,
    /// The most authoritative tier this provider may claim; items claiming
    /// better are demoted to it.
    #[serde(default = "default_external_tier")]
    pub max_tier: EvidenceTier,
    /// Model-derived output: every item is forced to the `model` tier.
    #[serde(default)]
    pub probabilistic: bool,
    /// Kind used for items that do not state one.
    #[serde(default = "default_external_kind")]
    pub default_kind: EvidenceKind,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_external_tier() -> EvidenceTier {
    EvidenceTier::StaticAnalysis
}

fn default_external_kind() -> EvidenceKind {
    EvidenceKind::Diagnostic
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceConfig {
    #[serde(default)]
    pub kaptaind: ToolConfig,
    #[serde(default)]
    pub traci: ToolConfig,
    #[serde(default)]
    pub fract: ToolConfig,
    #[serde(default)]
    pub zoc: ZocConfig,
    #[serde(default)]
    pub padagonia: PadagoniaConfig,
    /// Extra providers, by name. An entry named `sore` configures Sore.
    #[serde(default)]
    pub external: BTreeMap<String, ExternalProvider>,
}

impl EvidenceConfig {
    /// Load `<store>/evidence.json`, or defaults when absent.
    pub fn load(store_root: &Path) -> Result<Self, String> {
        let path = store_root.join("evidence.json");
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

/// Resolve a tool: an explicit `bin`, else `name` on `PATH`.
pub fn resolve_bin(name: &str, bin: Option<&Path>) -> Option<PathBuf> {
    if let Some(b) = bin {
        return b.is_file().then(|| b.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_offline_and_publish_nowhere() {
        let c = EvidenceConfig::default();
        assert!(c.kaptaind.enabled && c.traci.enabled && c.fract.enabled);
        assert_eq!(c.zoc.mode, "dry-run");
        assert!(!c.padagonia.enabled);
        assert!(c.external.is_empty());
    }

    #[test]
    fn partial_config_parses() {
        let c: EvidenceConfig = serde_json::from_str(
            r#"{"zoc": {"mode": "model"}, "traci": {"enabled": false},
                "external": {"sore": {"name": "sore", "argv": ["sore", "propose"], "probabilistic": true, "default_kind": "repair"}}}"#,
        )
        .unwrap();
        assert_eq!(c.zoc.mode, "model");
        assert!(c.zoc.tool.enabled);
        assert!(!c.traci.enabled);
        assert!(c.external["sore"].probabilistic);
        assert_eq!(c.external["sore"].max_tier, EvidenceTier::StaticAnalysis);
    }
}
