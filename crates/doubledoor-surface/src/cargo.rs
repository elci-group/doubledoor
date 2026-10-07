//! Cargo mutation analysis (§10, §47): what changed in a `Cargo.toml`,
//! structurally rather than line by line.
//!
//! Tracked: dependency tables (`dependencies`, `dev-dependencies`,
//! `build-dependencies`, `target.<cfg>.*`, `workspace.dependencies`),
//! `[features]`, `workspace.members`, and the package fields that change
//! how code compiles (`edition`, `rust-version`, `name`). Each change becomes
//! a surface `cargo:<manifest path>#<section>.<key>`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use doubledoor_core::SurfaceId;

/// What kind of manifest change occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CargoChangeKind {
    DependencyAdded,
    DependencyRemoved,
    DependencyChanged,
    FeatureAdded,
    FeatureRemoved,
    FeatureChanged,
    WorkspaceMemberAdded,
    WorkspaceMemberRemoved,
    PackageFieldChanged,
}

/// One structural manifest change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoChange {
    /// Repo-relative manifest path.
    pub manifest: String,
    pub surface: SurfaceId,
    pub kind: CargoChangeKind,
    /// `dependencies`, `target.'cfg(unix)'.dependencies`, `features`, ...
    pub section: String,
    /// Dependency key, feature name, member glob, or package field.
    pub name: String,
    pub before: Option<String>,
    pub after: Option<String>,
    /// For dependency changes: the version requirement moved across a
    /// semver-incompatible boundary (or could not be compared).
    pub semver_incompatible: bool,
}

impl CargoChange {
    /// The name code uses for a dependency (`serde-json` → `serde_json`;
    /// a renamed dependency's key is already its code name).
    pub fn code_name(&self) -> String {
        self.name.replace('-', "_")
    }

    pub fn is_dependency(&self) -> bool {
        matches!(
            self.kind,
            CargoChangeKind::DependencyAdded
                | CargoChangeKind::DependencyRemoved
                | CargoChangeKind::DependencyChanged
        )
    }
}

/// Every manifest change of one commit, plus manifests that did not parse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyDelta {
    pub changes: Vec<CargoChange>,
    /// `(manifest path, reason)`.
    pub unparsed: Vec<(String, String)>,
}

/// Diff two versions of one manifest (`None` = absent on that side).
pub fn diff_manifest(
    manifest: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<Vec<CargoChange>, String> {
    let parse = |s: Option<&str>| -> Result<toml::Value, String> {
        match s {
            Some(s) => toml::from_str(s).map_err(|e| e.message().to_string()),
            None => Ok(toml::Value::Table(Default::default())),
        }
    };
    let b = parse(before)?;
    let a = parse(after)?;
    let mut out = Vec::new();

    let bd = dependency_tables(&b);
    let ad = dependency_tables(&a);
    let sections: std::collections::BTreeSet<&String> = bd.keys().chain(ad.keys()).collect();
    for section in sections {
        let empty = BTreeMap::new();
        let before = bd.get(section).unwrap_or(&empty);
        let after = ad.get(section).unwrap_or(&empty);
        diff_keyed(manifest, section, before, after, true, &mut out);
    }

    let bf = table_entries(b.get("features"));
    let af = table_entries(a.get("features"));
    diff_keyed(manifest, "features", &bf, &af, false, &mut out);

    let bm = members(&b);
    let am = members(&a);
    for m in am.difference(&bm) {
        out.push(change(
            manifest,
            "workspace.members",
            m,
            CargoChangeKind::WorkspaceMemberAdded,
            None,
            Some(m.clone()),
        ));
    }
    for m in bm.difference(&am) {
        out.push(change(
            manifest,
            "workspace.members",
            m,
            CargoChangeKind::WorkspaceMemberRemoved,
            Some(m.clone()),
            None,
        ));
    }

    for field in ["name", "edition", "rust-version"] {
        let get = |v: &toml::Value| v.get("package").and_then(|p| p.get(field)).map(render);
        let (x, y) = (get(&b), get(&a));
        if x != y && (x.is_some() || y.is_some()) && before.is_some() && after.is_some() {
            out.push(change(
                manifest,
                "package",
                field,
                CargoChangeKind::PackageFieldChanged,
                x,
                y,
            ));
        }
    }
    Ok(out)
}

fn change(
    manifest: &str,
    section: &str,
    name: &str,
    kind: CargoChangeKind,
    before: Option<String>,
    after: Option<String>,
) -> CargoChange {
    CargoChange {
        manifest: manifest.to_string(),
        surface: SurfaceId::new(format!("cargo:{manifest}#{section}.{name}"))
            .expect("cargo surface ids are valid"),
        kind,
        section: section.to_string(),
        name: name.to_string(),
        before,
        after,
        semver_incompatible: false,
    }
}

fn diff_keyed(
    manifest: &str,
    section: &str,
    before: &BTreeMap<String, toml::Value>,
    after: &BTreeMap<String, toml::Value>,
    dependency: bool,
    out: &mut Vec<CargoChange>,
) {
    let (added, removed, changed) = if dependency {
        (
            CargoChangeKind::DependencyAdded,
            CargoChangeKind::DependencyRemoved,
            CargoChangeKind::DependencyChanged,
        )
    } else {
        (
            CargoChangeKind::FeatureAdded,
            CargoChangeKind::FeatureRemoved,
            CargoChangeKind::FeatureChanged,
        )
    };
    for (k, av) in after {
        match before.get(k) {
            None => out.push(change(manifest, section, k, added, None, Some(render(av)))),
            Some(bv) if bv != av => {
                let mut c = change(
                    manifest,
                    section,
                    k,
                    changed,
                    Some(render(bv)),
                    Some(render(av)),
                );
                if dependency {
                    c.semver_incompatible = match (version_of(bv), version_of(av)) {
                        (Some(x), Some(y)) => semver_incompatible(&x, &y),
                        (None, None) => false,
                        _ => true,
                    };
                }
                out.push(c);
            }
            Some(_) => {}
        }
    }
    for (k, bv) in before {
        if !after.contains_key(k) {
            out.push(change(
                manifest,
                section,
                k,
                removed,
                Some(render(bv)),
                None,
            ));
        }
    }
}

/// Every dependency table in a manifest, keyed by section name.
fn dependency_tables(v: &toml::Value) -> BTreeMap<String, BTreeMap<String, toml::Value>> {
    let mut out = BTreeMap::new();
    for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
        out.insert(kind.to_string(), table_entries(v.get(kind)));
        if let Some(targets) = v.get("target").and_then(toml::Value::as_table) {
            for (cfg, t) in targets {
                out.insert(format!("target.{cfg}.{kind}"), table_entries(t.get(kind)));
            }
        }
    }
    out.insert(
        "workspace.dependencies".to_string(),
        table_entries(v.get("workspace").and_then(|w| w.get("dependencies"))),
    );
    out.retain(|_, t| !t.is_empty());
    out
}

fn table_entries(v: Option<&toml::Value>) -> BTreeMap<String, toml::Value> {
    v.and_then(toml::Value::as_table)
        .map(|t| t.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

fn members(v: &toml::Value) -> std::collections::BTreeSet<String> {
    v.get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(toml::Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Compact rendering of a TOML value for evidence.
fn render(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A dependency's version requirement (`"1.2"` or `{ version = "1.2" }`).
fn version_of(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Table(t) => t
            .get("version")
            .and_then(toml::Value::as_str)
            .map(ToString::to_string),
        _ => None,
    }
}

/// Whether two version requirements fall in different semver-compatible
/// ranges (`1.x` vs `2.x`, `0.3` vs `0.4`, `0.0.1` vs `0.0.2`). Anything
/// unparseable counts as incompatible (unknown is not safe, §12).
pub fn semver_incompatible(before: &str, after: &str) -> bool {
    match (leading_version(before), leading_version(after)) {
        (Some(b), Some(a)) => compat_key(b) != compat_key(a),
        _ => true,
    }
}

fn leading_version(req: &str) -> Option<[u64; 3]> {
    let first = req.split(',').next()?.trim();
    let digits = first.trim_start_matches(['^', '=', '~', '>', '<', ' ']);
    let mut parts = [0u64; 3];
    let mut n = 0;
    for (i, p) in digits.split('.').take(3).enumerate() {
        let p = p.split(['-', '+']).next().unwrap_or(p);
        if p == "*" || p.is_empty() {
            break;
        }
        parts[i] = p.parse().ok()?;
        n += 1;
    }
    (n > 0).then_some(parts)
}

fn compat_key(v: [u64; 3]) -> (u64, u64, u64) {
    match v {
        [0, 0, p] => (0, 0, p),
        [0, m, _] => (0, m, 0),
        [maj, _, _] => (maj, 0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"
[package]
name = "router-app"
edition = "2021"

[dependencies]
serde = "1.0"
tokio = { version = "1", features = ["rt"] }
legacy-http = "0.3"

[features]
default = ["retry"]
retry = []
"#;

    const AFTER: &str = r#"
[package]
name = "router-app"
edition = "2024"

[dependencies]
serde = "1.0.200"
tokio = { version = "2", features = ["rt"] }
backoff = "0.4"

[features]
default = []
retry = []
metrics = []
"#;

    fn find<'a>(cs: &'a [CargoChange], name: &str, kind: CargoChangeKind) -> &'a CargoChange {
        cs.iter()
            .find(|c| c.name == name && c.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} for {name}: {cs:#?}"))
    }

    #[test]
    fn manifest_changes_are_structural() {
        let cs = diff_manifest("Cargo.toml", Some(BEFORE), Some(AFTER)).unwrap();
        let removed = find(&cs, "legacy-http", CargoChangeKind::DependencyRemoved);
        assert_eq!(removed.code_name(), "legacy_http");
        assert_eq!(
            removed.surface.as_str(),
            "cargo:Cargo.toml#dependencies.legacy-http"
        );
        find(&cs, "backoff", CargoChangeKind::DependencyAdded);
        assert!(!find(&cs, "serde", CargoChangeKind::DependencyChanged).semver_incompatible);
        assert!(find(&cs, "tokio", CargoChangeKind::DependencyChanged).semver_incompatible);
        find(&cs, "metrics", CargoChangeKind::FeatureAdded);
        find(&cs, "default", CargoChangeKind::FeatureChanged);
        let ed = find(&cs, "edition", CargoChangeKind::PackageFieldChanged);
        assert_eq!(ed.before.as_deref(), Some("2021"));
        assert_eq!(ed.after.as_deref(), Some("2024"));
        assert!(cs.iter().all(|c| c.name != "retry"), "unchanged feature");
    }

    #[test]
    fn whitespace_and_ordering_are_not_changes() {
        let reordered = "[dependencies]\ntokio={version=\"1\",features=[\"rt\"]}\nserde=\"1.0\"\nlegacy-http=\"0.3\"\n[package]\nname=\"router-app\"\nedition=\"2021\"\n[features]\nretry=[]\ndefault=[\"retry\"]\n";
        assert!(diff_manifest("Cargo.toml", Some(BEFORE), Some(reordered))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn workspace_members_and_dependencies() {
        let b = "[workspace]\nmembers = [\"a\"]\n[workspace.dependencies]\nsyn = \"1\"\n";
        let a = "[workspace]\nmembers = [\"a\", \"b\"]\n[workspace.dependencies]\nsyn = \"2\"\n";
        let cs = diff_manifest("Cargo.toml", Some(b), Some(a)).unwrap();
        find(&cs, "b", CargoChangeKind::WorkspaceMemberAdded);
        let syn = find(&cs, "syn", CargoChangeKind::DependencyChanged);
        assert_eq!(syn.section, "workspace.dependencies");
        assert!(syn.semver_incompatible);
    }

    #[test]
    fn semver_boundaries() {
        assert!(!semver_incompatible("1.2", "^1.9.3"));
        assert!(semver_incompatible("1", "2"));
        assert!(!semver_incompatible("0.3.1", "0.3.9"));
        assert!(semver_incompatible("0.3", "0.4"));
        assert!(semver_incompatible("0.0.1", "0.0.2"));
        assert!(semver_incompatible("*", "1"), "unparseable is not safe");
    }

    #[test]
    fn unparseable_manifest_is_an_error() {
        assert!(diff_manifest("Cargo.toml", Some("[x"), Some(AFTER)).is_err());
    }
}
