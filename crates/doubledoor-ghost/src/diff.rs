//! Textual delta parsing (§12, Phase II).
//!
//! A [`TextualDelta`] is the parsed form of a commit's unified diff:
//! which files changed, how, and where the hunks sit. Only `@@` hunk headers
//! are interpreted; body lines are counted structurally and never executed,
//! evaluated, or patched. Diffs whose output exceeds [`MAX_DIFF_BYTES`]
//! (10 MiB, §27) are rejected as [`DiffParseError::Oversized`] so the caller
//! can classify the observation `Unknown` — oversized must never mean safe.

use serde::{Deserialize, Serialize};

/// The §27 cap on diff output this layer will parse, in bytes.
pub const MAX_DIFF_BYTES: usize = 10 * 1024 * 1024;

/// How a file changed between the two commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
        };
        f.write_str(s)
    }
}

/// One `@@ -old_start,old_lines +new_start,new_lines @@` header. A range
/// written without a count (`@@ -5 +5,2 @@`) means a one-line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
}

/// One file's change with its hunk headers, in diff order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Repo-relative path of the new side (the path that survives the change;
    /// for deletions this is the path that disappeared).
    pub path: String,
    pub change: ChangeKind,
    pub hunks: Vec<Hunk>,
}

/// The full parsed delta for one commit-to-commit comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextualDelta {
    pub files: Vec<FileChange>,
}

/// Why a unified diff could not be parsed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiffParseError {
    /// The diff output exceeds the analysis cap (§27). The observation must
    /// be classified `Unknown` — never assumed safe.
    #[error("diff output is {bytes} bytes, exceeding the {max}-byte analysis cap")]
    Oversized { bytes: usize, max: usize },

    /// The output is not a well-formed unified diff (e.g. an unparseable
    /// `@@` header). Analysis failure, so the observation must be classified
    /// `Unknown`.
    #[error("malformed unified diff: {0}")]
    Malformed(String),
}

/// Parse unified diff output (from `git diff-tree -p` or `git diff <a> <b>`)
/// into a [`TextualDelta`]. Only structural elements are read: `diff --git`
/// file boundaries, file-level markers (`new file mode`, `deleted file
/// mode`, `Binary files ...`), and `@@` hunk headers. Enforces
/// [`MAX_DIFF_BYTES`].
pub fn parse_unified_diff(diff: &str) -> Result<TextualDelta, DiffParseError> {
    parse_unified_diff_limited(diff, MAX_DIFF_BYTES)
}

/// [`parse_unified_diff`] with an explicit cap, so tests can exercise the
/// oversized path without building a 10 MiB fixture.
pub fn parse_unified_diff_limited(
    diff: &str,
    max_bytes: usize,
) -> Result<TextualDelta, DiffParseError> {
    if diff.len() > max_bytes {
        return Err(DiffParseError::Oversized {
            bytes: diff.len(),
            max: max_bytes,
        });
    }
    let mut delta = TextualDelta::default();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            delta.files.push(start_file(rest)?);
        } else if line.starts_with("new file mode") {
            mark_change(&mut delta, ChangeKind::Added)?;
        } else if line.starts_with("deleted file mode") {
            mark_change(&mut delta, ChangeKind::Deleted)?;
        } else if line.starts_with("--- /dev/null") {
            mark_change(&mut delta, ChangeKind::Added)?;
        } else if line.starts_with("+++ /dev/null") {
            mark_change(&mut delta, ChangeKind::Deleted)?;
        } else if line.starts_with("@@ ") {
            let file = current_file(&mut delta, line)?;
            file.hunks.push(parse_hunk_header(line)?);
        }
        // Body lines (' ', '+', '-', '\') and other metadata (index, mode,
        // similarity) are intentionally not interpreted.
    }
    Ok(delta)
}

/// Extract the new-side path from a `diff --git a/<old> b/<new>` header line.
fn start_file(rest: &str) -> Result<FileChange, DiffParseError> {
    // The line is `a/<old> b/<new>`; paths with spaces appear quoted by git.
    let (_old, new) = rest
        .split_once(' ')
        .ok_or_else(|| DiffParseError::Malformed(format!("diff --git header: {rest}")))?;
    let path = new
        .strip_prefix("b/")
        .ok_or_else(|| DiffParseError::Malformed(format!("diff --git header: {rest}")))?;
    Ok(FileChange {
        path: path.to_string(),
        change: ChangeKind::Modified,
        hunks: Vec::new(),
    })
}

fn mark_change(delta: &mut TextualDelta, kind: ChangeKind) -> Result<(), DiffParseError> {
    current_file(delta, "file marker")?.change = kind;
    Ok(())
}

fn current_file<'a>(
    delta: &'a mut TextualDelta,
    context: &str,
) -> Result<&'a mut FileChange, DiffParseError> {
    delta
        .files
        .last_mut()
        .ok_or_else(|| DiffParseError::Malformed(format!("{context} before any file header")))
}

/// Parse one `@@ -old_start[,old_lines] +new_start[,new_lines] @@` header.
fn parse_hunk_header(line: &str) -> Result<Hunk, DiffParseError> {
    let rest = line
        .strip_prefix("@@ -")
        .ok_or_else(|| DiffParseError::Malformed(format!("hunk header: {line}")))?;
    let (old, rest) = rest
        .split_once(" +")
        .ok_or_else(|| DiffParseError::Malformed(format!("hunk header: {line}")))?;
    let (new, _) = rest
        .split_once(" @@")
        .ok_or_else(|| DiffParseError::Malformed(format!("hunk header: {line}")))?;
    let (old_start, old_lines) = parse_range(old)
        .ok_or_else(|| DiffParseError::Malformed(format!("hunk header old range: {line}")))?;
    let (new_start, new_lines) = parse_range(new)
        .ok_or_else(|| DiffParseError::Malformed(format!("hunk header new range: {line}")))?;
    Ok(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
    })
}

/// `start` or `start,count`; a bare start means a one-line range.
fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_added_file_with_hunks() {
        let diff = "\
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,3 @@
+pub fn one() {}
+pub fn two() {}
+pub fn three() {}
";
        let delta = parse_unified_diff(diff).unwrap();
        assert_eq!(delta.files.len(), 1);
        let f = &delta.files[0];
        assert_eq!(f.path, "src/new.rs");
        assert_eq!(f.change, ChangeKind::Added);
        assert_eq!(
            f.hunks,
            vec![Hunk {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines: 3
            }]
        );
    }

    #[test]
    fn parses_modified_file_with_multiple_hunks() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,7 +10,8 @@ fn a() {
 context
-removed
+added
 }
@@ -40,3 +41,5 @@ fn b() {
 tail
+more
+lines
 }
";
        let delta = parse_unified_diff(diff).unwrap();
        assert_eq!(delta.files.len(), 1);
        assert_eq!(delta.files[0].change, ChangeKind::Modified);
        assert_eq!(delta.files[0].hunks.len(), 2);
        assert_eq!(
            delta.files[0].hunks[0],
            Hunk {
                old_start: 10,
                old_lines: 7,
                new_start: 10,
                new_lines: 8
            }
        );
        assert_eq!(
            delta.files[0].hunks[1],
            Hunk {
                old_start: 40,
                old_lines: 3,
                new_start: 41,
                new_lines: 5
            }
        );
    }

    #[test]
    fn parses_deleted_file() {
        let diff = "\
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
index 3333333..0000000
--- a/gone.rs
+++ /dev/null
@@ -1,4 +0,0 @@
-old
-body
-lines
-end
";
        let delta = parse_unified_diff(diff).unwrap();
        assert_eq!(delta.files.len(), 1);
        assert_eq!(delta.files[0].path, "gone.rs");
        assert_eq!(delta.files[0].change, ChangeKind::Deleted);
        assert_eq!(
            delta.files[0].hunks,
            vec![Hunk {
                old_start: 1,
                old_lines: 4,
                new_start: 0,
                new_lines: 0
            }]
        );
    }

    #[test]
    fn hunk_header_without_count_means_one_line() {
        let diff = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -7 +9,2 @@\n";
        let delta = parse_unified_diff(diff).unwrap();
        assert_eq!(
            delta.files[0].hunks,
            vec![Hunk {
                old_start: 7,
                old_lines: 1,
                new_start: 9,
                new_lines: 2
            }]
        );
    }

    #[test]
    fn mode_only_change_has_no_hunks_but_is_tracked() {
        let diff = "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n";
        let delta = parse_unified_diff(diff).unwrap();
        assert_eq!(delta.files.len(), 1);
        assert_eq!(delta.files[0].change, ChangeKind::Modified);
        assert!(delta.files[0].hunks.is_empty());
    }

    #[test]
    fn binary_change_is_tracked_without_hunks() {
        let diff = "diff --git a/logo.png b/logo.png\nindex 4444444..5555555 100644\nBinary files a/logo.png and b/logo.png differ\n";
        let delta = parse_unified_diff(diff).unwrap();
        assert_eq!(delta.files.len(), 1);
        assert_eq!(delta.files[0].path, "logo.png");
        assert!(delta.files[0].hunks.is_empty());
    }

    #[test]
    fn empty_diff_yields_empty_delta() {
        let delta = parse_unified_diff("").unwrap();
        assert!(delta.files.is_empty());
    }

    #[test]
    fn malformed_hunk_header_is_rejected() {
        let diff = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ not a range @@\n";
        assert_eq!(
            parse_unified_diff(diff),
            Err(DiffParseError::Malformed(
                "hunk header: @@ not a range @@".to_string()
            ))
        );
    }

    #[test]
    fn marker_before_file_header_is_rejected() {
        assert_eq!(
            parse_unified_diff("new file mode 100644\n"),
            Err(DiffParseError::Malformed(
                "file marker before any file header".to_string()
            ))
        );
    }

    #[test]
    fn oversized_diff_is_rejected_with_limit() {
        let diff = format!("diff --git a/x b/x\n@@ -1 +1,2 @@\n{}", " ".repeat(64));
        assert_eq!(
            parse_unified_diff_limited(&diff, 32),
            Err(DiffParseError::Oversized {
                bytes: diff.len(),
                max: 32
            })
        );
        // The default cap admits ordinary diffs.
        assert!(parse_unified_diff(&diff).is_ok());
    }

    #[test]
    fn delta_serializes_for_persistence() {
        let diff = "diff --git a/x.rs b/x.rs\nnew file mode 100644\n--- /dev/null\n+++ b/x.rs\n@@ -0,0 +1 @@\n+x\n";
        let delta = parse_unified_diff(diff).unwrap();
        let json = serde_json::to_string(&delta).unwrap();
        let back: TextualDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(back, delta);
    }
}
