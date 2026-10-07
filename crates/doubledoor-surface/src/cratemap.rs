//! Where a Rust source file sits in its crate: the module path of a
//! repository path, derived from the Cargo manifests present.
//!
//! Conventions (Cargo's defaults; `#[path]` attributes and custom target
//! paths are not followed):
//!
//! - `<crate>/src/lib.rs`, `<crate>/src/main.rs` → `[crate]`
//! - `<crate>/src/a/b.rs`, `<crate>/src/a/b/mod.rs` → `[crate, a, b]`
//! - `<crate>/src/bin/x.rs`, `<crate>/src/bin/x/main.rs` → `[crate[bin:x]]`
//! - `<crate>/tests/t.rs` → `[crate[test:t]]`, likewise `examples`/`benches`
//! - `<crate>/build.rs` → `[crate[build]]`
//!
//! The crate segment is the library name (`[lib] name`, else the package
//! name with `-` → `_`), which is what `crate::`/external paths use.

use std::collections::BTreeSet;

/// One package found in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateRoot {
    /// Directory of the manifest, repo-relative (`""` for the root).
    pub dir: String,
    /// Code-level crate name.
    pub name: String,
}

/// Every package in a tree, plus manifests that failed to parse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrateMap {
    /// Sorted by directory depth, deepest first, so the innermost package
    /// owns a path.
    crates: Vec<CrateRoot>,
    /// Directories holding a `[workspace]` manifest.
    pub workspace_roots: BTreeSet<String>,
    /// `(manifest path, reason)` for manifests that could not be read.
    pub errors: Vec<(String, String)>,
}

impl CrateMap {
    /// Build from `(repo-relative manifest path, contents)` pairs.
    pub fn from_manifests<I, P, C>(manifests: I) -> Self
    where
        I: IntoIterator<Item = (P, C)>,
        P: AsRef<str>,
        C: AsRef<str>,
    {
        let mut map = CrateMap::default();
        for (path, contents) in manifests {
            let path = path.as_ref();
            let dir = manifest_dir(path);
            let value: toml::Value = match toml::from_str(contents.as_ref()) {
                Ok(v) => v,
                Err(e) => {
                    map.errors.push((path.to_string(), e.message().to_string()));
                    continue;
                }
            };
            if value.get("workspace").is_some() {
                map.workspace_roots.insert(dir.clone());
            }
            let lib_name = value
                .get("lib")
                .and_then(|l| l.get("name"))
                .and_then(toml::Value::as_str);
            let pkg_name = value
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(toml::Value::as_str);
            if let Some(name) = lib_name.or(pkg_name) {
                map.crates.push(CrateRoot {
                    dir,
                    name: name.replace('-', "_"),
                });
            }
        }
        map.crates.sort_by(|a, b| {
            depth(&b.dir)
                .cmp(&depth(&a.dir))
                .then_with(|| a.dir.cmp(&b.dir))
        });
        map
    }

    /// The innermost package containing `path`.
    pub fn crate_of(&self, path: &str) -> Option<&CrateRoot> {
        self.crates
            .iter()
            .find(|c| c.dir.is_empty() || path.starts_with(&format!("{}/", c.dir)))
    }

    /// The module path (`[crate, module...]`) of a `.rs` file, or `None` for
    /// non-Rust files and files outside any package.
    pub fn module_of(&self, path: &str) -> Option<Vec<String>> {
        let stem_path = path.strip_suffix(".rs")?;
        let root = self.crate_of(path)?;
        let rel = if root.dir.is_empty() {
            stem_path
        } else {
            &stem_path[root.dir.len() + 1..]
        };
        let parts: Vec<&str> = rel.split('/').collect();
        let name = root.name.clone();
        let module = match parts.as_slice() {
            ["src", "lib"] | ["src", "main"] => vec![name],
            ["src", "bin", bin] | ["src", "bin", bin, "main"] => vec![format!("{name}[bin:{bin}]")],
            ["src", rest @ ..] => {
                let mut m = vec![name];
                let mut rest: Vec<&str> = rest.to_vec();
                if rest.last() == Some(&"mod") {
                    rest.pop();
                }
                m.extend(rest.iter().map(ToString::to_string));
                m
            }
            ["build"] => vec![format!("{name}[build]")],
            [kind @ ("tests" | "examples" | "benches"), rest @ ..] if !rest.is_empty() => {
                let label = kind.trim_end_matches('s');
                let mut m = vec![format!("{name}[{label}:{}]", rest[0])];
                let mut tail: Vec<&str> = rest[1..].to_vec();
                if tail.last() == Some(&"mod") || tail.last() == Some(&"main") {
                    tail.pop();
                }
                m.extend(tail.iter().map(ToString::to_string));
                m
            }
            _ => return None,
        };
        Some(module)
    }
}

/// The directory of a manifest path (`crates/x/Cargo.toml` → `crates/x`).
pub fn manifest_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default()
}

fn depth(dir: &str) -> usize {
    if dir.is_empty() {
        0
    } else {
        dir.split('/').count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> CrateMap {
        CrateMap::from_manifests([
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            (
                "crates/router-app/Cargo.toml",
                "[package]\nname = \"router-app\"\n",
            ),
            (
                "crates/other/Cargo.toml",
                "[package]\nname = \"other\"\n[lib]\nname = \"renamed\"\n",
            ),
        ])
    }

    fn m(path: &str) -> Option<Vec<String>> {
        map().module_of(path)
    }

    fn v(segs: &[&str]) -> Option<Vec<String>> {
        Some(segs.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn module_paths_follow_cargo_conventions() {
        assert_eq!(m("crates/router-app/src/lib.rs"), v(&["router_app"]));
        assert_eq!(
            m("crates/router-app/src/router.rs"),
            v(&["router_app", "router"])
        );
        assert_eq!(
            m("crates/router-app/src/net/mod.rs"),
            v(&["router_app", "net"])
        );
        assert_eq!(
            m("crates/router-app/src/net/tcp.rs"),
            v(&["router_app", "net", "tcp"])
        );
        assert_eq!(
            m("crates/router-app/src/bin/cli.rs"),
            v(&["router_app[bin:cli]"])
        );
        assert_eq!(
            m("crates/router-app/tests/api.rs"),
            v(&["router_app[test:api]"])
        );
        assert_eq!(m("crates/router-app/build.rs"), v(&["router_app[build]"]));
        assert_eq!(m("crates/other/src/lib.rs"), v(&["renamed"]));
    }

    #[test]
    fn non_rust_and_unowned_paths_have_no_module() {
        assert_eq!(m("crates/router-app/README.md"), None);
        // The root manifest is a virtual workspace: no package owns src/.
        assert_eq!(m("scripts/tool.rs"), None);
        assert!(map().workspace_roots.contains(""));
    }

    #[test]
    fn broken_manifests_are_reported_not_guessed() {
        let map = CrateMap::from_manifests([("Cargo.toml", "[package\nname=")]);
        assert_eq!(map.errors.len(), 1);
        assert_eq!(map.module_of("src/lib.rs"), None);
    }
}
