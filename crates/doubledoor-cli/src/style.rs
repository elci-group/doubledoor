//! Human-output styling, built on `form3` (ANSI styling, tables, terminal
//! capability detection). Color is opt-in by environment: a TTY stdout with
//! no `NO_COLOR`/`TERM=dumb` gets color; anything piped, redirected, or run
//! under `NO_COLOR` gets byte-identical plain text, so the §31 machine-
//! readable shapes and every scripted consumer are unaffected. JSON output
//! never passes through this module.

use std::io::IsTerminal;
use std::sync::OnceLock;

use form3::ansi::Color;
use form3::compat::{Colorize, StyledText};
use form3::table::{Attribute, Cell};
use form3::term::ColorSupport;

/// Color support for stdout (where all human renders land).
fn stdout_support() -> ColorSupport {
    static SUPPORT: OnceLock<ColorSupport> = OnceLock::new();
    *SUPPORT.get_or_init(|| ColorSupport::from_env(std::io::stdout().is_terminal()))
}

/// Color support for stderr (errors and warnings).
fn stderr_support() -> ColorSupport {
    static SUPPORT: OnceLock<ColorSupport> = OnceLock::new();
    *SUPPORT.get_or_init(|| ColorSupport::from_env(std::io::stderr().is_terminal()))
}

fn styled(text: impl std::fmt::Display, support: ColorSupport) -> StyledText {
    StyledText::new(text.to_string()).with_color_support(support)
}

fn paint_out<F>(text: impl std::fmt::Display, f: F) -> String
where
    F: FnOnce(StyledText) -> StyledText,
{
    f(styled(text, stdout_support())).to_string()
}

fn paint_err<F>(text: impl std::fmt::Display, f: F) -> String
where
    F: FnOnce(StyledText) -> StyledText,
{
    f(styled(text, stderr_support())).to_string()
}

// ---------------------------------------------------------------- stdout —

/// A `KEY` in a `KEY=VALUE` pair.
pub fn key(text: impl std::fmt::Display) -> String {
    paint_out(text, |s| s.bright_cyan().bold())
}

/// De-emphasised text: hashes, paths, counters, supporting detail.
pub fn dim(text: impl std::fmt::Display) -> String {
    paint_out(text, |s| s.bright_black())
}

/// Emphasised text: ids, subject lines.
pub fn strong(text: impl std::fmt::Display) -> String {
    paint_out(text, |s| s.bold())
}

/// Green for healthy/affirmative words.
pub fn good(text: impl std::fmt::Display) -> String {
    paint_out(text, |s| s.bright_green())
}

/// Red for broken/blocked words.
pub fn bad(text: impl std::fmt::Display) -> String {
    paint_out(text, |s| s.bright_red().bold())
}

/// Yellow for attention words (pending, concerns, skipped).
pub fn caution(text: impl std::fmt::Display) -> String {
    paint_out(text, |s| s.bright_yellow())
}

/// Magenta for informational-but-noteworthy words.
pub fn info(text: impl std::fmt::Display) -> String {
    paint_out(text, |s| s.bright_magenta())
}

/// A bold bright-cyan word, for the strongest emphasis on stdout.
pub fn accent(text: impl std::fmt::Display) -> String {
    paint_out(text, |s| s.bright_cyan().bold())
}

/// Whether stdout renders accept ANSI (used to gate always-colored table
/// cells, which have no per-stream detection of their own).
pub fn color_on() -> bool {
    stdout_support().supports_color()
}

// ---------------------------------------------------------------- stderr —

/// The `doubledoor:` prefix of a human error line.
pub fn error_prefix() -> String {
    paint_err("doubledoor:", |s| s.bright_red().bold())
}

/// The `doubledoor:` prefix of a human warning line.
pub fn warn_prefix() -> String {
    paint_err("doubledoor:", |s| s.bright_yellow().bold())
}

// ------------------------------------------------------------ semantics —

/// A lifecycle state, colored by its mood: live states green, undecided
/// states yellow, in-flight states blue, terminal-but-broken states red,
/// terminal-but-fine states bright.
pub fn state(text: impl std::fmt::Display) -> String {
    let name = text.to_string();
    let upper = name.to_ascii_uppercase();
    let mood = if upper.contains("FAIL") || upper.contains("INVALID") {
        bad(name) as String
    } else {
        match upper.as_str() {
            "ACTIVE" | "COMPLETED" => good(name),
            "GHOST_PENDING" | "ASSESSING" => caution(name),
            "REQUESTED" | "ISOLATING" => accent(name),
            "ABANDONED" => dim(name),
            _ => info(name),
        }
    };
    mood
}

/// A ghost classification, colored by severity: breaking and unknown are
/// red, overlap is yellow, relevance is magenta/cyan, independent is green.
pub fn classification(text: impl std::fmt::Display) -> String {
    let name = text.to_string();
    match name.as_str() {
        "api_breaking" | "unknown" => bad(name),
        "write_overlap" => caution(name),
        "read_relevant" => accent(name),
        "dependency_relevant" => info(name),
        "independent" => good(name),
        _ => strong(name),
    }
}

/// A ghost assessment status.
pub fn ghost_status(text: impl std::fmt::Display) -> String {
    let name = text.to_string();
    match name.as_str() {
        "pending" => caution(name),
        "deferred" => accent(name),
        "accepted" => good(name),
        "rejected" => bad(name),
        _ => strong(name),
    }
}

/// A unified-evidence verdict.
pub fn verdict(text: impl std::fmt::Display) -> String {
    let name = text.to_string();
    match name.as_str() {
        "BLOCKED" | "blocked" => bad(name),
        "CONCERNS" | "concern" => caution(name),
        "UNVERIFIED" | "unverified" => accent(name),
        "CLEAR" | "clear" | "supports" => good(name),
        "info" => dim(name),
        _ => strong(name),
    }
}

/// A validation-gate status word, with a leading marker glyph.
pub fn gate_status(text: impl std::fmt::Display) -> String {
    let name = text.to_string();
    let lowered = name.to_ascii_lowercase();
    if lowered.starts_with("pass") {
        format!("{} {}", marker_ok(), good(name))
    } else if lowered.starts_with("fail") {
        format!("{} {}", marker_failed(), bad(name))
    } else {
        format!("{} {}", marker_skipped(), caution(name))
    }
}

// -------------------------------------------------------------- markers —

/// ✓ for a passing check or gate.
pub fn marker_ok() -> String {
    good("✓")
}

/// ✗ for a failing check or gate.
pub fn marker_failed() -> String {
    bad("✗")
}

/// ○ for a neutral/skipped item.
pub fn marker_skipped() -> String {
    caution("○")
}

/// The colored bullet before an indented list item.
pub fn bullet() -> String {
    dim("·")
}

// ------------------------------------------------------------- sections —

/// Render width for rules: `COLUMNS`, else 80, clamped to a readable band.
fn rule_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|w| w.parse::<usize>().ok())
        .unwrap_or(80)
        .clamp(40, 120)
}

/// A section rule: `── TITLE ─────────…` with the title bold.
pub fn section(title: &str) -> String {
    let fill = rule_width()
        .saturating_sub(title.chars().count() + 7)
        .max(3);
    format!(
        "{}{}{}",
        dim("── "),
        strong(title),
        dim(format!(" {}", "─".repeat(fill)))
    )
}

/// A complete list item line: indented bullet plus content.
pub fn item(content: impl std::fmt::Display) -> String {
    format!("  {} {}", bullet(), content)
}

/// An indented continuation line (e.g. a diagnostic under its item).
pub fn sub_item(content: impl std::fmt::Display) -> String {
    format!("      {}", dim(content))
}

// ---------------------------------------------------------------- table —

/// A table cell, colored only when stdout renders color (cells always emit
/// their sequences, so gating happens here at construction).
pub fn cell(text: impl std::fmt::Display, fg: Option<Color>) -> Cell {
    let mut cell = Cell::new(text.to_string());
    if color_on() {
        if let Some(fg) = fg {
            cell = cell.fg(fg);
        }
    }
    cell
}

/// A bold table cell (typically a header), colored only when color is on.
pub fn header_cell(text: impl std::fmt::Display) -> Cell {
    let mut cell = Cell::new(text.to_string());
    if color_on() {
        cell = cell.add_attribute(Attribute::Bold);
    }
    cell
}

/// The severity color for a table cell, from a state/classification/verdict
/// string.
pub fn severity_color(text: &str) -> Option<Color> {
    let upper = text.to_ascii_uppercase();
    if upper.contains("FAIL") || upper.contains("BREAK") || upper.contains("BLOCK") {
        Some(Color::Red)
    } else if upper.contains("PENDING") || upper.contains("CONCERN") || upper.contains("OVERLAP") {
        Some(Color::Yellow)
    } else if upper == "ACTIVE" || upper.contains("CLEAR") || upper.contains("PASS") {
        Some(Color::Green)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_keep_their_title_plain_and_contiguous() {
        let rendered = section("ACTION REQUIRED");
        assert!(rendered.contains("── ACTION REQUIRED ──"));
    }

    #[test]
    fn items_are_indented_under_a_bullet() {
        assert!(item("file:alpha.txt").starts_with("  · file:alpha.txt"));
    }
}
