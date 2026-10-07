//! Deterministic sets of mutation surfaces (§11).
//!
//! [`SurfaceSet`] backs the read/write/dependency sets of a DoorSession and
//! the surface arithmetic of ghost classification. It is a `BTreeSet`
//! internally so iteration, intersection, and every serialization are
//! deterministic (§3) — the same inputs always render in the same order.

use std::collections::BTreeSet;
use std::iter::FromIterator;

use serde::{Deserialize, Serialize};

use crate::SurfaceId;

/// An ordered, deduplicated collection of surfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceSet(BTreeSet<SurfaceId>);

impl SurfaceSet {
    pub fn new() -> Self {
        SurfaceSet(BTreeSet::new())
    }

    /// Insert a surface. Returns `true` when it was not already present.
    pub fn insert(&mut self, surface: SurfaceId) -> bool {
        self.0.insert(surface)
    }

    pub fn contains(&self, surface: &SurfaceId) -> bool {
        self.0.contains(surface)
    }

    /// `true` when the two sets share no surface.
    pub fn is_disjoint(&self, other: &SurfaceSet) -> bool {
        self.0.is_disjoint(&other.0)
    }

    /// The surfaces present in both sets.
    pub fn intersection(&self, other: &SurfaceSet) -> SurfaceSet {
        SurfaceSet(self.0.intersection(&other.0).cloned().collect())
    }

    /// Every surface present in either set.
    pub fn union(&self, other: &SurfaceSet) -> SurfaceSet {
        SurfaceSet(self.0.union(&other.0).cloned().collect())
    }

    /// Surfaces in lexicographic (deterministic) order.
    pub fn iter(&self) -> impl Iterator<Item = &SurfaceId> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<SurfaceId> for SurfaceSet {
    fn from_iter<I: IntoIterator<Item = SurfaceId>>(iter: I) -> Self {
        SurfaceSet(iter.into_iter().collect())
    }
}

impl<'a> IntoIterator for &'a SurfaceSet {
    type Item = &'a SurfaceId;
    type IntoIter = std::collections::btree_set::Iter<'a, SurfaceId>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> SurfaceId {
        SurfaceId::new(s).unwrap()
    }

    fn set(items: &[&str]) -> SurfaceSet {
        items.iter().map(|s| id(s)).collect()
    }

    #[test]
    fn insert_deduplicates_and_reports_presence() {
        let mut s = SurfaceSet::new();
        assert!(s.is_empty());
        assert!(s.insert(id("file:a.rs")));
        assert!(!s.insert(id("file:a.rs")));
        s.insert(id("file:b.rs"));
        assert_eq!(s.len(), 2);
        assert!(s.contains(&id("file:a.rs")));
        assert!(!s.contains(&id("file:c.rs")));
    }

    #[test]
    fn iteration_order_is_deterministic() {
        let s = set(&["file:z.rs", "file:a.rs", "file:m.rs"]);
        let ordered: Vec<&str> = s.iter().map(|i| i.as_str()).collect();
        assert_eq!(ordered, vec!["file:a.rs", "file:m.rs", "file:z.rs"]);
    }

    #[test]
    fn intersection_and_union() {
        let a = set(&["file:a.rs", "file:b.rs", "file:c.rs"]);
        let b = set(&["file:b.rs", "file:c.rs", "file:d.rs"]);
        let inter = a.intersection(&b);
        assert_eq!(inter.len(), 2);
        assert!(inter.contains(&id("file:b.rs")));
        assert!(inter.contains(&id("file:c.rs")));
        assert!(!inter.contains(&id("file:a.rs")));

        let union = a.union(&b);
        assert_eq!(union.len(), 4);
        assert!(union.contains(&id("file:d.rs")));
    }

    #[test]
    fn is_disjoint_reflects_shared_surfaces() {
        let a = set(&["file:a.rs"]);
        let b = set(&["file:b.rs"]);
        let c = set(&["file:a.rs", "file:d.rs"]);
        assert!(a.is_disjoint(&b));
        assert!(!a.is_disjoint(&c));
    }

    #[test]
    fn from_iterator_builds_a_set() {
        let s: SurfaceSet = vec![id("file:x.rs"), id("file:x.rs"), id("file:y.rs")]
            .into_iter()
            .collect();
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn serialization_roundtrip_preserves_order() {
        let s = set(&["file:b.rs", "file:a.rs"]);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"["file:a.rs","file:b.rs"]"#);
        let back: SurfaceSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
