//! Optional ELCI evidence providers (§21, §49). Each adapter runs its tool
//! as an argument vector (never a shell) with a timeout, parses the tool's
//! real output, and fails closed: unrecognised output makes the run
//! `Failed` and contributes no items — it is never guessed at. A tool that
//! is not installed is `Unavailable`; Doubledoor works the same without it.
//!
//! A non-zero exit is *not* a failure by itself: analysers conventionally
//! exit non-zero when they find something (Traci exits 2 with findings).
//! Any run that completes is judged by whether its output parses; only
//! timeouts and launch errors fail outright. The exit code is kept on each
//! item as provenance.
//!
//! | provider | runs | evidence | tier |
//! |---|---|---|---|
//! | Traci | `traci check . --format json` | diagnostics on promoted files | static analysis |
//! | Fract | `fract --format json index` | architecture health of promoted modules | static analysis |
//! | Kaptaind | `kaptaind analyze -r <tree>` (candidate staged over its target) | API break / projected semver bump, dependency score | static analysis |
//! | ZOC | `zoc --task <intent> . --dry-run` | where the actor's intent matters | heuristic (`model` with LLM scoring) |
//! | external (e.g. Sore) | configured argv, `doubledoor.evidence/v1` JSON | any, capped | ≤ configured cap; `model` when probabilistic |
//!
//! No external provider can *block*: a `blocks` verdict from outside is
//! demoted to `concern`. Only Doubledoor's own compiler, test, and
//! integrity evidence demonstrate problems.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;

use doubledoor_core::evidence::{
    EvidenceItem, EvidenceKind as K, EvidenceTier as T, ProviderRun, ProviderStatus, Verdict as V,
    EVIDENCE_SCHEMA,
};
use doubledoor_validate::{run_command, GateResult, GateStatus};

use crate::config::{resolve_bin, ExternalProvider, ToolConfig, ZocConfig};

/// Where and for what an adapter runs.
pub struct Target<'a> {
    /// Worktree with the candidate checked out.
    pub tree: &'a Path,
    /// Worktree at the target with the candidate's changes staged (for
    /// tools that analyse "uncommitted changes").
    pub staged: &'a Path,
    /// Files the promotion changes.
    pub files: &'a [String],
    pub intent: &'a str,
    pub logs: &'a Path,
    /// JSON request describing the candidate (external protocol).
    pub request: &'a Path,
    pub env: &'a [(&'a str, &'a str)],
}

pub type Outcome = (Vec<EvidenceItem>, ProviderRun);

fn run(
    name: &str,
    tier: Option<T>,
    status: ProviderStatus,
    started: Instant,
    argv: &[String],
) -> ProviderRun {
    ProviderRun {
        provider: name.to_string(),
        tier,
        status,
        duration_ms: started.elapsed().as_millis() as u64,
        command: argv.to_vec(),
    }
}

/// Resolve and execute; `Err` carries the finished `ProviderRun`.
fn execute(
    name: &str,
    tier: Option<T>,
    cfg: &ToolConfig,
    args: &[String],
    cwd: &Path,
    t: &Target<'_>,
) -> Result<(GateResult, Vec<String>, Instant), ProviderRun> {
    let started = Instant::now();
    if !cfg.enabled {
        return Err(run(name, tier, ProviderStatus::Disabled, started, &[]));
    }
    let Some(bin) = resolve_bin(name, cfg.bin.as_deref()) else {
        return Err(run(
            name,
            tier,
            ProviderStatus::Unavailable {
                reason: format!("`{name}` is not installed (not on PATH)"),
            },
            started,
            &[],
        ));
    };
    let mut argv = vec![bin.to_string_lossy().into_owned()];
    argv.extend(args.iter().cloned());
    let mut env = vec![("NO_COLOR", "1")];
    env.extend_from_slice(t.env);
    let res = run_command(
        name,
        &argv,
        cwd,
        &env,
        t.logs,
        name,
        Duration::from_secs(cfg.timeout_secs),
    )
    .map_err(|e| {
        run(
            name,
            tier,
            ProviderStatus::Failed {
                reason: e.to_string(),
            },
            started,
            &argv,
        )
    })?;
    match &res.status {
        GateStatus::Passed | GateStatus::Failed { .. } => Ok((res, argv, started)),
        other => Err(run(
            name,
            tier,
            ProviderStatus::Failed {
                reason: format!(
                    "{other:?}: {}",
                    res.stderr_tail.lines().last().unwrap_or_default()
                ),
            },
            started,
            &argv,
        )),
    }
}

/// The process exit code of a completed run (provenance attribute).
fn exit_code(res: &GateResult) -> String {
    match &res.status {
        GateStatus::Failed { code } => code.map_or("signal".to_string(), |c| c.to_string()),
        _ => "0".to_string(),
    }
}

fn full_stdout(t: &Target<'_>, name: &str, res: &GateResult) -> String {
    std::fs::read_to_string(t.logs.join(format!("{name}.out")))
        .unwrap_or_else(|_| res.stdout_tail.clone())
}

fn failed(
    name: &str,
    tier: Option<T>,
    started: Instant,
    argv: &[String],
    reason: String,
) -> Outcome {
    (
        Vec::new(),
        run(name, tier, ProviderStatus::Failed { reason }, started, argv),
    )
}

fn ran(
    name: &str,
    tier: Option<T>,
    started: Instant,
    argv: &[String],
    items: Vec<EvidenceItem>,
) -> Outcome {
    let n = items.len();
    (
        items,
        run(name, tier, ProviderStatus::Ran { items: n }, started, argv),
    )
}

fn relpath(p: &str) -> String {
    p.trim_start_matches("./").to_string()
}

// ---------------------------------------------------------------------------
// Traci
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TraciReport {
    diagnostics: Vec<TraciDiag>,
    summary: serde_json::Value,
}

#[derive(Deserialize)]
struct TraciDiag {
    rule: String,
    severity: String,
    path: String,
    line: Option<usize>,
    message: String,
}

pub fn traci(cfg: &ToolConfig, t: &Target<'_>) -> Outcome {
    let tier = Some(T::StaticAnalysis);
    let args: Vec<String> = ["check", ".", "--format", "json"]
        .map(String::from)
        .to_vec();
    let (res, argv, started) = match execute("traci", tier, cfg, &args, t.tree, t) {
        Ok(x) => x,
        Err(r) => return (Vec::new(), r),
    };
    let code = exit_code(&res);
    let out = full_stdout(t, "traci", &res);
    let report: TraciReport = match serde_json::from_str(out.trim()) {
        Ok(r) => r,
        Err(e) => {
            return failed(
                "traci",
                tier,
                started,
                &argv,
                format!("unrecognised output: {e}"),
            )
        }
    };
    let mut items = Vec::new();
    for d in report
        .diagnostics
        .iter()
        .filter(|d| t.files.contains(&relpath(&d.path)))
    {
        let verdict = match d.severity.as_str() {
            "critical" | "error" | "warning" => V::Concern,
            _ => V::Info,
        };
        let path = relpath(&d.path);
        items.push(
            EvidenceItem::new(
                "traci",
                K::Diagnostic,
                T::StaticAnalysis,
                verdict,
                Some(format!("file:{path}")),
                format!("[{}] {}", d.rule, d.message),
            )
            .at(path, d.line)
            .attr("severity", &d.severity)
            .by(&argv)
            .attr("exit_code", &code),
        );
    }
    items.push(
        EvidenceItem::new(
            "traci",
            K::Diagnostic,
            T::StaticAnalysis,
            V::Info,
            None,
            format!("repository-wide: {}", report.summary),
        )
        .by(&argv)
        .attr("exit_code", &code),
    );
    ran("traci", tier, started, &argv, items)
}

// ---------------------------------------------------------------------------
// Fract
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct FractReport {
    schema: String,
    summary: serde_json::Value,
    findings: Vec<FractFinding>,
}

#[derive(Deserialize)]
struct FractFinding {
    severity: String,
    module: String,
    message: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    entropy: Option<f64>,
}

pub fn fract(cfg: &ToolConfig, t: &Target<'_>) -> Outcome {
    let tier = Some(T::StaticAnalysis);
    let args: Vec<String> = ["--format", "json", "--no-animation", "--no-color", "index"]
        .map(String::from)
        .to_vec();
    let (res, argv, started) = match execute("fract", tier, cfg, &args, t.tree, t) {
        Ok(x) => x,
        Err(r) => return (Vec::new(), r),
    };
    let code = exit_code(&res);
    let out = full_stdout(t, "fract", &res);
    let report: FractReport = match serde_json::from_str(out.trim()) {
        Ok(r) => r,
        Err(e) => {
            return failed(
                "fract",
                tier,
                started,
                &argv,
                format!("unrecognised output: {e}"),
            )
        }
    };
    if !report.schema.starts_with("fract.report/") {
        return failed(
            "fract",
            tier,
            started,
            &argv,
            format!("unsupported schema {}", report.schema),
        );
    }
    let mut items = Vec::new();
    for f in report
        .findings
        .iter()
        .filter(|f| t.files.contains(&relpath(&f.module)))
    {
        let verdict = match f.severity.as_str() {
            "warning" | "critical" => V::Concern,
            _ => V::Info,
        };
        let path = relpath(&f.module);
        let mut item = EvidenceItem::new(
            "fract",
            K::Architecture,
            T::StaticAnalysis,
            verdict,
            Some(format!("file:{path}")),
            format!("{}: {}", f.kind, f.message),
        )
        .at(path, None)
        .attr("severity", &f.severity)
        .by(&argv)
        .attr("exit_code", &code);
        if let Some(e) = f.entropy {
            item = item.attr("entropy", format!("{e:.3}"));
        }
        items.push(item);
    }
    items.push(
        EvidenceItem::new(
            "fract",
            K::Architecture,
            T::StaticAnalysis,
            V::Info,
            None,
            format!("{} summary: {}", report.schema, report.summary),
        )
        .by(&argv)
        .attr("exit_code", &code),
    );
    ran("fract", tier, started, &argv, items)
}

// ---------------------------------------------------------------------------
// Kaptaind
// ---------------------------------------------------------------------------

/// Parsed `kaptaind analyze` output (it has no JSON mode; its labelled lines
/// are parsed, and anything missing fails the run).
#[derive(Debug, PartialEq)]
pub struct KaptaindAnalysis {
    pub api_break: bool,
    pub api_added: bool,
    pub api_score: String,
    pub deps_score: String,
    pub runtime_score: String,
    pub total_score: String,
    pub bump: String,
}

pub fn parse_kaptaind(out: &str) -> Option<KaptaindAnalysis> {
    let value = |label: &str| -> Option<String> {
        out.lines()
            .find(|l| l.contains(label))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
    };
    let yes_no = |v: String| match v.as_str() {
        "Yes" => Some(true),
        "No" => Some(false),
        _ => None,
    };
    Some(KaptaindAnalysis {
        api_break: yes_no(value("API Break:")?)?,
        api_added: yes_no(value("API Added:")?)?,
        api_score: value("API Score:")?,
        deps_score: value("Deps Score:")?,
        runtime_score: value("Runtime Score:")?,
        total_score: value("Total Score:")?,
        // "📈 Projected Bump: 📌 Stable" → "Stable"
        bump: value("Projected Bump:")?
            .split_whitespace()
            .last()?
            .to_string(),
    })
}

pub fn kaptaind(cfg: &ToolConfig, t: &Target<'_>) -> Outcome {
    let tier = Some(T::StaticAnalysis);
    let staged = t.staged.to_string_lossy().into_owned();
    let args = vec!["analyze".to_string(), "-r".to_string(), staged];
    let (res, argv, started) = match execute("kaptaind", tier, cfg, &args, t.staged, t) {
        Ok(x) => x,
        Err(r) => return (Vec::new(), r),
    };
    let code = exit_code(&res);
    let out = full_stdout(t, "kaptaind", &res);
    let Some(a) = parse_kaptaind(&out) else {
        return failed(
            "kaptaind",
            tier,
            started,
            &argv,
            "unrecognised `kaptaind analyze` output".to_string(),
        );
    };
    let mut items = vec![
        EvidenceItem::new(
            "kaptaind",
            K::Api,
            T::StaticAnalysis,
            if a.api_break { V::Concern } else { V::Supports },
            None,
            if a.api_break {
                "kaptaind reports a public API break".to_string()
            } else {
                "kaptaind reports no public API break".to_string()
            },
        )
        .attr("api_score", &a.api_score)
        .attr("api_added", a.api_added)
        .by(&argv)
        .attr("exit_code", &code),
        EvidenceItem::new(
            "kaptaind",
            K::Api,
            T::StaticAnalysis,
            V::Info,
            None,
            format!(
                "projected semver bump: {} (total score {})",
                a.bump, a.total_score
            ),
        )
        .attr("bump", &a.bump)
        .by(&argv)
        .attr("exit_code", &code),
    ];
    items.push(
        EvidenceItem::new(
            "kaptaind",
            K::Dependency,
            T::StaticAnalysis,
            V::Info,
            None,
            format!(
                "dependency score {}, runtime score {}",
                a.deps_score, a.runtime_score
            ),
        )
        .by(&argv)
        .attr("exit_code", &code),
    );
    ran("kaptaind", tier, started, &argv, items)
}

// ---------------------------------------------------------------------------
// ZOC
// ---------------------------------------------------------------------------

/// One ranked row of `zoc` output: `#1   60  medium   crates/x   reason`.
#[derive(Debug, PartialEq)]
pub struct ZocRow {
    pub rank: u32,
    pub score: u32,
    pub band: String,
    pub path: String,
    pub reason: String,
}

pub fn parse_zoc(out: &str) -> Vec<ZocRow> {
    out.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix('#')?;
            let mut it = rest.split_whitespace();
            let rank = it.next()?.parse().ok()?;
            let score = it.next()?.parse().ok()?;
            let band = it.next()?.to_string();
            let path = it.next()?.to_string();
            let reason = it.collect::<Vec<_>>().join(" ");
            Some(ZocRow {
                rank,
                score,
                band,
                path,
                reason,
            })
        })
        .collect()
}

pub fn zoc(cfg: &ZocConfig, t: &Target<'_>) -> Outcome {
    let model = cfg.mode == "model";
    let tier_v = if model { T::Model } else { T::Heuristic };
    let tier = Some(tier_v);
    if model && std::env::var_os("GROQ_API_KEY").is_none() {
        return (
            Vec::new(),
            run(
                "zoc",
                tier,
                ProviderStatus::Unavailable {
                    reason: "model mode needs GROQ_API_KEY".into(),
                },
                Instant::now(),
                &[],
            ),
        );
    }
    let mut args: Vec<String> = vec![
        "--task".into(),
        t.intent.to_string(),
        ".".into(),
        "--plain".into(),
        "--no-animate".into(),
        "--limit".into(),
        "5".into(),
    ];
    if !model {
        args.push("--dry-run".into());
    }
    let (res, argv, started) = match execute("zoc", tier, &cfg.tool, &args, t.tree, t) {
        Ok(x) => x,
        Err(r) => return (Vec::new(), r),
    };
    let code = exit_code(&res);
    let rows = parse_zoc(&full_stdout(t, "zoc", &res));
    if rows.is_empty() {
        return failed(
            "zoc",
            tier,
            started,
            &argv,
            "no ranked rows in output".to_string(),
        );
    }
    let items = rows
        .iter()
        .map(|r| {
            let touched = t
                .files
                .iter()
                .filter(|f| r.path == "." || f.starts_with(&format!("{}/", r.path)))
                .count();
            EvidenceItem::new(
                "zoc",
                K::Relevance,
                tier_v,
                V::Info,
                Some(format!("dir:{}", r.path)),
                format!(
                    "#{} {} ({} {}) for the actor's intent; {touched} promoted file(s) inside — {}",
                    r.rank, r.path, r.score, r.band, r.reason
                ),
            )
            .attr("score", r.score)
            .attr("band", &r.band)
            .by(&argv)
            .attr("exit_code", &code)
        })
        .collect();
    ran("zoc", tier, started, &argv, items)
}

// ---------------------------------------------------------------------------
// Generic external protocol (Sore and anything else)
// ---------------------------------------------------------------------------

/// What an external provider prints on stdout.
#[derive(Deserialize)]
struct ExternalReport {
    schema: String,
    items: Vec<ExternalItem>,
}

#[derive(Deserialize)]
struct ExternalItem {
    #[serde(default)]
    kind: Option<K>,
    #[serde(default)]
    tier: Option<T>,
    verdict: V,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<usize>,
    summary: String,
}

/// Run a `doubledoor.evidence/v1` provider. It receives the candidate via
/// environment (`DOUBLEDOOR_CANDIDATE_TREE`, `DOUBLEDOOR_REQUEST`, …) and
/// prints `{"schema": "doubledoor.evidence/v1", "items": [...]}`.
pub fn external(p: &ExternalProvider, t: &Target<'_>) -> Outcome {
    let cap = if p.probabilistic {
        T::Model
    } else {
        p.max_tier
    };
    let tier = Some(cap);
    let started = Instant::now();
    let Some((prog, args)) = p.argv.split_first() else {
        return failed(&p.name, tier, started, &p.argv, "empty argv".into());
    };
    let cfg = ToolConfig {
        enabled: true,
        bin: Some(PathBuf::from(prog)).filter(|b| b.components().count() > 1),
        timeout_secs: p.timeout_secs,
    };
    let lookup_name = Path::new(prog)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(prog);
    let (res, argv, started) = match execute(lookup_name, tier, &cfg, args, t.tree, t) {
        Ok(x) => x,
        Err(mut r) => {
            r.provider = p.name.clone();
            return (Vec::new(), r);
        }
    };
    let code = exit_code(&res);
    let out = full_stdout(t, lookup_name, &res);
    let report: ExternalReport = match serde_json::from_str(out.trim()) {
        Ok(r) => r,
        Err(e) => {
            return failed(
                &p.name,
                tier,
                started,
                &argv,
                format!("not {EVIDENCE_SCHEMA} JSON: {e}"),
            )
        }
    };
    if report.schema != EVIDENCE_SCHEMA {
        return failed(
            &p.name,
            tier,
            started,
            &argv,
            format!("unsupported schema {}", report.schema),
        );
    }
    let items = report
        .items
        .into_iter()
        .map(|i| {
            // Never more authoritative than the configured cap; never blocks.
            let claimed = i.tier.unwrap_or(cap);
            let tier = claimed.max(cap);
            let verdict = if i.verdict == V::Blocks {
                V::Concern
            } else {
                i.verdict
            };
            let mut item = EvidenceItem::new(
                &p.name,
                i.kind.unwrap_or(p.default_kind),
                tier,
                verdict,
                i.subject,
                i.summary,
            )
            .by(&argv)
            .attr("exit_code", &code);
            if let Some(f) = i.file {
                item = item.at(f, i.line);
            }
            if claimed < cap {
                item = item.attr("claimed_tier", format!("{claimed:?}"));
            }
            item
        })
        .collect();
    ran(&p.name, tier, started, &argv, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_kaptaind_output() {
        let out = "🧪 Dry-run Analysis Result:\n-----------------------------------\n🗂️ Touched Paths: 1\n💥 API Break:     No\n➕ API Added:     No\n🔌 API Score:     0.194\n📦 Deps Score:    0.000\n⚙️ Runtime Score: 0.000\n-----------------------------------\n🎯 Total Score:   0.073\n📈 Projected Bump: 📌 Stable\n";
        let a = parse_kaptaind(out).unwrap();
        assert!(!a.api_break);
        assert_eq!(a.api_score, "0.194");
        assert_eq!(a.bump, "Stable");
        assert!(
            parse_kaptaind("something else entirely").is_none(),
            "fail closed"
        );
        assert!(
            parse_kaptaind(&out.replace("API Break:     No", "API Break:     Maybe")).is_none()
        );
    }

    #[test]
    fn parses_real_zoc_output() {
        let out = "Zone of Confidence - x\n... scanning\n\nResults\n-----\n #1   60  medium   crates/doubledoor-reconcile   3 aim term hits, 1 path hits\n #2   42  low      .                             3 aim term hits, 0 path hits\n";
        let rows = parse_zoc(out);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, "crates/doubledoor-reconcile");
        assert_eq!(rows[0].score, 60);
        assert_eq!(rows[1].reason, "3 aim term hits, 0 path hits");
    }
}
