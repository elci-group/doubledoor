//! Deterministic validation of reconciliation candidates (§22, §48 Phase IV).
//!
//! The pipeline runs against a throwaway detached worktree of the candidate
//! commit — never the session's workspace, never the canonical worktree:
//!
//! ```text
//! git integrity → cargo metadata → cargo check → cargo test → configured checks
//! ```
//!
//! The Rust gates run when the candidate has a root `Cargo.toml`. Every gate
//! is an argument vector (never a shell), runs with a timeout, and writes its
//! output to log files (so a chatty build can never deadlock on a pipe). The
//! first failing required gate stops the pipeline; later gates are recorded
//! as skipped, never as passed. The report is the persisted evidence.
//!
//! Validation executes the candidate's own build scripts and tests — code
//! from the repository, which Doubledoor already treats as developer-
//! controlled (§27). It runs with the invoking user's privileges.

#![forbid(unsafe_code)]

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use doubledoor_core::GitOid;
use doubledoor_git::Git;

/// Bytes of each gate's stdout/stderr kept in the report.
const TAIL_BYTES: u64 = 4096;

/// One configured gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateSpec {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default = "default_gate_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "yes")]
    pub required: bool,
}

fn default_gate_timeout() -> u64 {
    600
}

fn yes() -> bool {
    true
}

/// `.doubledoor/validation.json` (all fields optional).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationConfig {
    /// Run the built-in Rust profile when the candidate has a `Cargo.toml`.
    #[serde(default = "yes")]
    pub rust: bool,
    /// Pass `--offline` to cargo (no network during validation).
    #[serde(default)]
    pub cargo_offline: bool,
    /// Run `cargo test` (after `cargo check`).
    #[serde(default = "yes")]
    pub cargo_test: bool,
    /// Per-gate timeout for the built-in gates.
    #[serde(default = "default_gate_timeout")]
    pub cargo_timeout_secs: u64,
    /// Repository-specific checks, run after the built-in gates.
    #[serde(default)]
    pub gates: Vec<GateSpec>,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        ValidationConfig {
            rust: true,
            cargo_offline: false,
            cargo_test: true,
            cargo_timeout_secs: default_gate_timeout(),
            gates: Vec::new(),
        }
    }
}

impl ValidationConfig {
    /// Load `<store>/validation.json`, or the defaults when absent.
    pub fn load(store_root: &Path) -> Result<Self, ValidateError> {
        let path = store_root.join("validation.json");
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| ValidateError::Config(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
}

/// How one gate ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GateStatus {
    Passed,
    Failed {
        code: Option<i32>,
    },
    TimedOut {
        after_secs: u64,
    },
    Skipped {
        reason: String,
    },
    /// The gate could not be started (e.g. `cargo` not installed).
    Error {
        message: String,
    },
}

impl GateStatus {
    pub fn passed(&self) -> bool {
        matches!(self, GateStatus::Passed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    pub name: String,
    pub argv: Vec<String>,
    pub required: bool,
    #[serde(flatten)]
    pub status: GateStatus,
    pub duration_ms: u64,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

/// A compiler error reported by `cargo check` (JSON messages).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Repo-relative path.
    pub file: String,
    pub line: usize,
    pub level: String,
    pub code: Option<String>,
    pub message: String,
}

/// The persisted outcome of one validation run (§22).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    pub commit: GitOid,
    pub passed: bool,
    pub gates: Vec<GateResult>,
    pub diagnostics: Vec<Diagnostic>,
    /// `cargo check` ran to completion, so diagnostics are complete.
    pub compiled: bool,
    pub started_at_millis: u64,
    pub finished_at_millis: u64,
}

#[derive(Debug, Error)]
pub enum ValidateError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Git(#[from] doubledoor_git::GitError),
    #[error("invalid validation config: {0}")]
    Config(String),
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Validate `commit`. `work` is a scratch directory owned by the caller
/// (the candidate worktree and logs live under it and are removed after);
/// `target_dir` is the cargo target directory to reuse across runs of the
/// same session; `budget` caps the whole run (§28 maximum validation
/// runtime).
pub fn run(
    git: &Git,
    commit: &GitOid,
    parent: &GitOid,
    work: &Path,
    target_dir: &Path,
    cfg: &ValidationConfig,
    budget: Duration,
) -> Result<ValidationReport, ValidateError> {
    let started = now_ms();
    let deadline = Instant::now() + budget;
    let logs = work.join("logs");
    fs::create_dir_all(&logs)?;
    let tree = work.join("tree");
    let mut gates = Vec::new();
    let mut diagnostics = Vec::new();
    let mut compiled = false;

    // 1. Git integrity: every object the candidate adds is present.
    let t = Instant::now();
    let integrity = git.run_git_status(&[
        "rev-list",
        "--objects",
        "--missing=error",
        commit.as_str(),
        "--not",
        parent.as_str(),
    ])?;
    gates.push(GateResult {
        name: "git integrity".to_string(),
        argv: vec![
            "git".into(),
            "rev-list".into(),
            "--objects".into(),
            "--missing=error".into(),
            commit.to_string(),
            "--not".into(),
            parent.to_string(),
        ],
        required: true,
        status: if integrity.0 == 0 {
            GateStatus::Passed
        } else {
            GateStatus::Failed {
                code: Some(integrity.0),
            }
        },
        duration_ms: t.elapsed().as_millis() as u64,
        stdout_tail: String::new(),
        stderr_tail: integrity.2.clone(),
    });

    // Materialize the candidate privately.
    let _ = git.run_git(&["worktree", "remove", "--force", path_str(&tree)]);
    let _ = fs::remove_dir_all(&tree);
    let _ = git.run_git(&["worktree", "prune"]);
    let materialized = gates[0].status.passed()
        && git
            .run_git(&[
                "worktree",
                "add",
                "--detach",
                "-q",
                path_str(&tree),
                commit.as_str(),
            ])
            .is_ok();

    let mut plan: Vec<GateSpec> = Vec::new();
    if cfg.rust && tree.join("Cargo.toml").is_file() {
        let offline: Vec<String> = if cfg.cargo_offline {
            vec!["--offline".into()]
        } else {
            Vec::new()
        };
        let cargo = |args: &[&str]| -> Vec<String> {
            let mut v: Vec<String> = std::iter::once("cargo".to_string())
                .chain(args.iter().map(ToString::to_string))
                .collect();
            v.extend(offline.iter().cloned());
            v
        };
        plan.push(GateSpec {
            name: "cargo metadata".into(),
            argv: cargo(&["metadata", "--format-version", "1", "--no-deps"]),
            timeout_secs: cfg.cargo_timeout_secs,
            required: true,
        });
        plan.push(GateSpec {
            name: "cargo check".into(),
            argv: cargo(&[
                "check",
                "--workspace",
                "--all-targets",
                "--message-format=json",
            ]),
            timeout_secs: cfg.cargo_timeout_secs,
            required: true,
        });
        if cfg.cargo_test {
            plan.push(GateSpec {
                name: "cargo test".into(),
                argv: cargo(&["test", "--workspace"]),
                timeout_secs: cfg.cargo_timeout_secs,
                required: true,
            });
        }
    }
    plan.extend(cfg.gates.iter().cloned());

    let mut stop: Option<String> = if !gates[0].status.passed() {
        Some("git integrity failed".to_string())
    } else if !materialized {
        Some("the candidate could not be materialized".to_string())
    } else {
        None
    };
    for (i, spec) in plan.iter().enumerate() {
        if let Some(reason) = &stop {
            gates.push(skipped(spec, reason));
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            stop = Some("validation runtime budget exhausted".to_string());
            gates.push(skipped(spec, stop.as_deref().unwrap_or_default()));
            continue;
        }
        let timeout = Duration::from_secs(spec.timeout_secs).min(remaining);
        let res = run_gate(spec, &tree, target_dir, &logs, i, timeout)?;
        if spec.name == "cargo check" {
            let out = fs::read_to_string(logs.join(format!("{i}.out"))).unwrap_or_default();
            diagnostics = parse_cargo_json(&out);
            compiled = matches!(res.status, GateStatus::Passed | GateStatus::Failed { .. });
        }
        if spec.required && !res.status.passed() {
            stop = Some(format!("required gate `{}` did not pass", spec.name));
        }
        gates.push(res);
    }

    let _ = git.run_git(&["worktree", "remove", "--force", path_str(&tree)]);
    let _ = fs::remove_dir_all(&tree);
    let _ = git.run_git(&["worktree", "prune"]);

    let passed = gates.iter().all(|g| !g.required || g.status.passed());
    Ok(ValidationReport {
        commit: commit.clone(),
        passed,
        gates,
        diagnostics,
        compiled,
        started_at_millis: started,
        finished_at_millis: now_ms(),
    })
}

fn skipped(spec: &GateSpec, reason: &str) -> GateResult {
    GateResult {
        name: spec.name.clone(),
        argv: spec.argv.clone(),
        required: spec.required,
        status: GateStatus::Skipped {
            reason: reason.to_string(),
        },
        duration_ms: 0,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
    }
}

fn run_gate(
    spec: &GateSpec,
    cwd: &Path,
    target_dir: &Path,
    logs: &Path,
    index: usize,
    timeout: Duration,
) -> Result<GateResult, ValidateError> {
    let target = target_dir.to_string_lossy().into_owned();
    let mut res = run_command(
        &spec.name,
        &spec.argv,
        cwd,
        &[("CARGO_TARGET_DIR", target.as_str())],
        logs,
        &index.to_string(),
        timeout,
    )?;
    res.required = spec.required;
    Ok(res)
}

/// Run one argument vector (never a shell) in `cwd` with extra `env`,
/// stdout/stderr to `<logs>/<log_name>.out|.err`, killed at `timeout`.
/// The returned result carries the status and output tails; the full
/// output stays in the log files. Used by the gates and by evidence
/// providers alike.
pub fn run_command(
    name: &str,
    argv: &[String],
    cwd: &Path,
    env: &[(&str, &str)],
    logs: &Path,
    log_name: &str,
    timeout: Duration,
) -> Result<GateResult, ValidateError> {
    fs::create_dir_all(logs)?;
    let (out_path, err_path) = (
        logs.join(format!("{log_name}.out")),
        logs.join(format!("{log_name}.err")),
    );
    let started = Instant::now();
    let base = GateResult {
        name: name.to_string(),
        argv: argv.to_vec(),
        required: true,
        status: GateStatus::Passed,
        duration_ms: 0,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
    };
    let Some((program, args)) = argv.split_first() else {
        return Ok(GateResult {
            status: GateStatus::Error {
                message: "empty argv".to_string(),
            },
            ..base
        });
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(File::create(&out_path)?)
        .stderr(File::create(&err_path)?);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ok(GateResult {
                status: GateStatus::Error {
                    message: format!("cannot start `{program}`: {e}"),
                },
                ..base
            })
        }
    };
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break if st.success() {
                GateStatus::Passed
            } else {
                GateStatus::Failed { code: st.code() }
            };
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            break GateStatus::TimedOut {
                after_secs: timeout.as_secs(),
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(GateResult {
        status,
        duration_ms: started.elapsed().as_millis() as u64,
        stdout_tail: tail(&out_path),
        stderr_tail: tail(&err_path),
        ..base
    })
}

fn tail(path: &PathBuf) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Errors from `cargo check --message-format=json` output: one per primary
/// span of each `compiler-message` with level `error`.
pub fn parse_cargo_json(out: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for line in out.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-message" {
            continue;
        }
        let msg = &v["message"];
        let level = msg["level"].as_str().unwrap_or_default();
        if level != "error" {
            continue;
        }
        let Some(spans) = msg["spans"].as_array() else {
            continue;
        };
        for span in spans.iter().filter(|s| s["is_primary"] == true) {
            diags.push(Diagnostic {
                file: span["file_name"].as_str().unwrap_or_default().to_string(),
                line: span["line_start"].as_u64().unwrap_or(0) as usize,
                level: level.to_string(),
                code: msg["code"]["code"].as_str().map(ToString::to_string),
                message: msg["message"].as_str().unwrap_or_default().to_string(),
            });
        }
    }
    diags
}

fn path_str(p: &Path) -> &str {
    p.to_str().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_primary_error_spans_only() {
        let out = r#"{"reason":"compiler-artifact","package_id":"x"}
{"reason":"compiler-message","message":{"level":"warning","message":"unused","spans":[{"file_name":"src/a.rs","line_start":1,"is_primary":true}],"code":null}}
{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/router.rs","line_start":12,"is_primary":true},{"file_name":"src/config.rs","line_start":3,"is_primary":false}]}}
not json
"#;
        let d = parse_cargo_json(out);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].file, "src/router.rs");
        assert_eq!(d[0].line, 12);
        assert_eq!(d[0].code.as_deref(), Some("E0308"));
    }

    #[test]
    fn config_defaults_and_overrides() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ValidationConfig::load(dir.path()).unwrap(),
            ValidationConfig::default()
        );
        fs::write(
            dir.path().join("validation.json"),
            r#"{"cargo_test": false, "gates": [{"name": "lint", "argv": ["true"]}]}"#,
        )
        .unwrap();
        let c = ValidationConfig::load(dir.path()).unwrap();
        assert!(!c.cargo_test && c.rust);
        assert_eq!(c.gates[0].timeout_secs, 600);
        assert!(c.gates[0].required);
    }

    #[test]
    fn gates_time_out_fail_and_error_without_masquerading_as_passes() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        let spec = |argv: &[&str]| GateSpec {
            name: "g".into(),
            argv: argv.iter().map(ToString::to_string).collect(),
            timeout_secs: 1,
            required: true,
        };
        let run = |s: &GateSpec| {
            run_gate(
                s,
                dir.path(),
                dir.path(),
                &logs,
                0,
                Duration::from_millis(300),
            )
            .unwrap()
        };
        assert!(run(&spec(&["true"])).status.passed());
        assert_eq!(
            run(&spec(&["false"])).status,
            GateStatus::Failed { code: Some(1) }
        );
        assert!(matches!(
            run(&spec(&["sleep", "5"])).status,
            GateStatus::TimedOut { .. }
        ));
        assert!(matches!(
            run(&spec(&["no-such-binary-dd"])).status,
            GateStatus::Error { .. }
        ));
        let echo = run(&spec(&["echo", "hello"]));
        assert_eq!(echo.stdout_tail.trim(), "hello");
    }
}
