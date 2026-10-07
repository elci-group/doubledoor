//! Output rendering: human KEY=VALUE/column rendering and `--format json`
//! emission via the protocol view types (§31/§32). All rendering lives here;
//! `commands.rs` only builds the view values.

use std::io::{self, Write};

use clap::ValueEnum;
use serde::Serialize;

use doubledoor_protocol::{
    DecisionView, DoctorReport, GhostView, RecoveryReport, SequencedEvent, SessionObservation,
    SessionSummary, StatusReport, SurfacesView,
};

use crate::error::CliError;
use crate::style;

/// Output format for every command (`--format`, default human).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputFormat::Human => f.write_str("human"),
            OutputFormat::Json => f.write_str("json"),
        }
    }
}

/// Serialize a view value as pretty JSON to stdout.
pub fn print_json(value: &impl Serialize) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, value)
        .map_err(|e| CliError::Internal(e.to_string()))?;
    handle
        .write_all(b"\n")
        .map_err(|e| CliError::Internal(e.to_string()))?;
    Ok(())
}

fn kv(key: &str, value: impl std::fmt::Display) {
    println!("{}={value}", style::key(key));
}

/// The sessions of a status/sessions listing, as a table.
fn sessions_table(sessions: &[SessionSummary]) {
    if sessions.is_empty() {
        println!("{}", style::dim("no sessions"));
        return;
    }
    let mut table = form3::table::Table::new();
    table.set_style(form3::table::TableStyle::Square);
    table.set_header(vec![
        style::header_cell("ID"),
        style::header_cell("STATE"),
        style::header_cell("ACTOR"),
        style::header_cell("INTENT"),
    ]);
    for s in sessions {
        let state = s.state.to_string();
        table.add_row(vec![
            style::cell(s.id, None),
            style::cell(&state, style::severity_color(&state)),
            style::cell(&s.actor, None),
            style::cell(&s.intent, None),
        ]);
    }
    println!("{table}");
}

/// `open` / `abandon` share the §31 machine-readable shape:
/// `SESSION=… STATE=… WORKSPACE=…`.
pub fn render_open(format: OutputFormat, summary: &SessionSummary) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(summary),
        OutputFormat::Human => {
            println!("{}", style::section("SESSION"));
            kv("SESSION", style::strong(summary.id));
            kv("STATE", style::state(summary.state));
            if let Some(ws) = &summary.workspace {
                kv("WORKSPACE", style::dim(ws.display()));
            }
            Ok(())
        }
    }
}

/// One session per line: id, state, actor, intent.
pub fn render_sessions(format: OutputFormat, sessions: &[SessionSummary]) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(&sessions),
        OutputFormat::Human => {
            sessions_table(sessions);
            Ok(())
        }
    }
}

pub fn render_status(format: OutputFormat, report: &StatusReport) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(report),
        OutputFormat::Human => {
            println!("{}", style::section("REPOSITORY"));
            kv("REPO", report.repo_root.display());
            kv("SCHEMA", style::dim(&report.schema));
            kv(
                "ACTIVE",
                if report.active > 0 {
                    style::caution(report.active)
                } else {
                    style::good(0)
                },
            );
            kv("TOTAL", report.total);
            println!();
            println!("{}", style::section("SESSIONS"));
            sessions_table(&report.sessions);
            Ok(())
        }
    }
}

/// Full session detail: the summary plus journal and workspace reality.
pub fn render_inspect(
    format: OutputFormat,
    summary: &SessionSummary,
    journal_events: usize,
    workspace_exists: bool,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => {
            let mut value =
                serde_json::to_value(summary).map_err(|e| CliError::Internal(e.to_string()))?;
            let obj = value
                .as_object_mut()
                .expect("SessionSummary serializes to an object");
            obj.insert(
                "journal_events".to_string(),
                serde_json::json!(journal_events),
            );
            obj.insert(
                "workspace_exists".to_string(),
                serde_json::json!(workspace_exists),
            );
            print_json(&value)
        }
        OutputFormat::Human => {
            println!("{}", style::section("SESSION"));
            kv("SESSION", style::strong(summary.id));
            kv("ACTOR", &summary.actor);
            kv("INTENT", &summary.intent);
            kv("STATE", style::state(summary.state));
            kv("BASE_COMMIT", style::dim(&summary.base_commit));
            kv("CREATED_AT_MILLIS", summary.created_at_millis);
            match &summary.workspace {
                Some(ws) => kv("WORKSPACE", style::dim(ws.display())),
                None => kv("WORKSPACE", style::dim("<none>")),
            }
            kv(
                "WORKSPACE_EXISTS",
                if workspace_exists {
                    style::good("true")
                } else {
                    style::bad("false")
                },
            );
            kv("JOURNAL_EVENTS", journal_events);
            kv(
                "SURFACES",
                style::dim(format!("[{}]", summary.requested_surfaces.join(", "))),
            );
            Ok(())
        }
    }
}

pub fn render_recover(format: OutputFormat, report: &RecoveryReport) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(report),
        OutputFormat::Human => {
            for action in &report.actions {
                let mark = if action.action.contains("FAIL") || action.detail.contains("FAIL") {
                    style::marker_failed()
                } else if action.action.contains("torn") || action.action.contains("prune") {
                    style::marker_skipped()
                } else {
                    style::marker_ok()
                };
                println!(
                    "{mark} {}  {}  {}",
                    style::strong(action.session),
                    action.action,
                    style::dim(&action.detail)
                );
            }
            println!();
            kv("SCANNED", report.sessions_scanned);
            kv("ACTIONS", report.actions.len());
            Ok(())
        }
    }
}

pub fn render_doctor(format: OutputFormat, report: &DoctorReport) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(report),
        OutputFormat::Human => {
            for check in &report.checks {
                println!(
                    "{}  {}  {}",
                    if check.ok {
                        style::marker_ok()
                    } else {
                        style::marker_failed()
                    },
                    if check.ok {
                        style::strong(&check.name)
                    } else {
                        style::bad(&check.name)
                    },
                    style::dim(&check.detail)
                );
            }
            let failed = report.checks.iter().filter(|c| !c.ok).count();
            println!();
            if failed == 0 {
                println!(
                    "{} {}",
                    style::marker_ok(),
                    style::good(format!("all {} checks passed", report.checks.len()))
                );
            } else {
                println!(
                    "{} {}",
                    style::marker_failed(),
                    style::bad(format!("{failed} of {} checks failed", report.checks.len()))
                );
            }
            Ok(())
        }
    }
}

/// One line per observed session. JSON output is one object per line so a
/// long-running `watch` can be consumed incrementally.
pub fn render_observations(
    format: OutputFormat,
    observed: &[SessionObservation],
) -> Result<(), CliError> {
    for obs in observed {
        match format {
            OutputFormat::Json => print_json_line(obs)?,
            OutputFormat::Human => println!(
                "{}{} {}{} {}{} {}{}",
                style::key("SESSION="),
                style::strong(&obs.session),
                style::key("STATE="),
                style::state(obs.state.to_string()),
                style::key("NEW_GHOSTS="),
                if obs.new_ghosts.is_empty() {
                    style::good(0)
                } else {
                    style::caution(obs.new_ghosts.len())
                },
                style::key("CURSOR="),
                style::dim(&obs.cursor),
            ),
        }
    }
    Ok(())
}

/// One stream event per line: JSON lines, or a compact human line.
pub fn render_event(format: OutputFormat, ev: &SequencedEvent) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json_line(ev),
        OutputFormat::Human => {
            let v = serde_json::to_value(ev).map_err(|e| CliError::Internal(e.to_string()))?;
            let name = v["event"].as_str().unwrap_or("?").to_string();
            let detail = match &ev.event {
                doubledoor_protocol::StreamEvent::GhostAvailable {
                    ghost,
                    classification,
                    ..
                } => format!(
                    "{}{} {}{}",
                    style::key("GHOST="),
                    style::dim(ghost),
                    style::key("CLASSIFICATION="),
                    style::classification(classification)
                ),
                doubledoor_protocol::StreamEvent::GhostAssessed {
                    ghost, decision, ..
                } => format!(
                    "{}{} {}{}",
                    style::key("GHOST="),
                    style::dim(ghost),
                    style::key("DECISION="),
                    style::ghost_status(decision)
                ),
                doubledoor_protocol::StreamEvent::SessionState { from, to, .. } => format!(
                    "{}{} {}{}",
                    style::key("FROM="),
                    style::dim(from.to_string()),
                    style::key("TO="),
                    style::state(to.to_string())
                ),
                doubledoor_protocol::StreamEvent::CandidateBuilt {
                    candidate,
                    promotable,
                    ..
                } => format!(
                    "{}{} {}{}",
                    style::key("CANDIDATE="),
                    style::dim(candidate),
                    style::key("PROMOTABLE="),
                    if *promotable {
                        style::good("true")
                    } else {
                        style::caution("false")
                    }
                ),
                doubledoor_protocol::StreamEvent::CandidateValidated {
                    candidate, passed, ..
                } => format!(
                    "{}{} {}{}",
                    style::key("CANDIDATE="),
                    style::dim(candidate),
                    style::key("PASSED="),
                    if *passed {
                        style::good("true")
                    } else {
                        style::bad("false")
                    }
                ),
                doubledoor_protocol::StreamEvent::CandidatePromoted {
                    candidate,
                    to,
                    partial,
                    ..
                } => format!(
                    "{}{} {}{} {}{}",
                    style::key("CANDIDATE="),
                    style::dim(candidate),
                    style::key("CANONICAL="),
                    style::dim(to),
                    style::key("PARTIAL="),
                    if *partial {
                        style::caution("true")
                    } else {
                        style::good("false")
                    }
                ),
            };
            let event_word = if name.starts_with("ghost.available") {
                style::accent(&name)
            } else if name.starts_with("ghost.assessed") {
                style::info(&name)
            } else if name.starts_with("candidate") {
                style::caution(&name)
            } else {
                style::state(&name)
            };
            println!(
                "{}{} {}{} {}{} {detail}",
                style::key("SEQ="),
                style::dim(ev.seq),
                style::key("EVENT="),
                event_word,
                style::key("SESSION="),
                style::strong(ev.event.session()),
            );
            Ok(())
        }
    }
}

/// The §31 ghost block, answering the §32 questions: what changed, who
/// changed it, why it matters, what accepting imports, what stays yours.
pub fn render_ghosts(format: OutputFormat, ghosts: &[GhostView]) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(&ghosts),
        OutputFormat::Human => {
            for (i, g) in ghosts.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                println!("{}", style::section(&format!("GHOST {}", g.id)));
                println!(
                    "{} {}",
                    style::key("STATUS"),
                    style::ghost_status(&g.status)
                );
                println!(
                    "{} {}",
                    style::key("CLASSIFICATION"),
                    style::classification(&g.classification)
                );
                println!();
                println!("{}", style::section("SOURCE"));
                println!("{}", g.source_actor.as_deref().unwrap_or("<unknown actor>"));
                println!("{}", style::dim(&g.source_commit.as_str()[..12]));
                if let Some(intent) = &g.intent {
                    println!();
                    println!("{}", style::section("INTENT"));
                    println!("{intent}");
                }
                block("CHANGED", &g.affected);
                block("INTERSECTION", &g.intersections);
                block("SAFE IMPORTS", &g.safe_imports);
                block("BREAKING IMPORTS", &g.breaking_imports);
                block("HELD", &g.held);
                let relevant: Vec<String> = g
                    .assessments
                    .iter()
                    .filter(|a| a.classification != "independent")
                    .map(|a| {
                        format!(
                            "{}  {}  {}",
                            a.surface,
                            style::classification(&a.classification),
                            style::dim(&a.reason)
                        )
                    })
                    .collect();
                block("ASSESSMENT", &relevant);
                let broken: Vec<String> = g
                    .invalidated_assumptions
                    .iter()
                    .map(|v| format!("{} {}", style::marker_failed(), v.description))
                    .collect();
                block("ASSUMPTION INVALIDATED", &broken);
                if let Some(commit) = &g.applied_commit {
                    println!();
                    println!("{}", style::section("APPLIED AS"));
                    println!("{}", style::dim(commit));
                }
                if g.needs_assessment {
                    println!();
                    println!("{}", style::section("ACTION REQUIRED"));
                    println!(
                        "{} {} {} {} {}",
                        style::good("accept"),
                        style::dim("|"),
                        style::accent("defer"),
                        style::dim("|"),
                        style::bad("reject"),
                    );
                    if !g.held.is_empty() && !g.safe_imports.is_empty() {
                        let args: Vec<String> = g
                            .safe_imports
                            .iter()
                            .map(|s| format!("--surface {s}"))
                            .collect();
                        println!(
                            "{} {}",
                            style::dim("keep your held surfaces:"),
                            style::dim(format!("doubledoor accept {} {}", g.id, args.join(" ")))
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

fn block(title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    println!();
    println!("{}", style::section(title));
    for item in items {
        println!("{}", style::item(item));
    }
}

pub fn render_decision(format: OutputFormat, d: &DecisionView) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(d),
        OutputFormat::Human => {
            println!("{}", style::section("DECISION"));
            kv("GHOST", style::strong(d.ghost));
            kv("DECISION", style::ghost_status(&d.decision));
            kv("SESSION", d.session);
            kv("STATE", style::state(d.state));
            if let Some(commit) = &d.applied_commit {
                kv("APPLIED_COMMIT", style::dim(commit));
            }
            if !d.surfaces.is_empty() {
                kv("IMPORTED", style::good(d.surfaces.join(",")));
            }
            kv(
                "OUTSTANDING",
                if d.outstanding == 0 {
                    style::good(0)
                } else {
                    style::caution(d.outstanding)
                },
            );
            Ok(())
        }
    }
}

/// Compact single-line JSON (for streams).
fn print_json_line(value: &impl Serialize) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, value).map_err(|e| CliError::Internal(e.to_string()))?;
    handle
        .write_all(b"\n")
        .and_then(|()| handle.flush())
        .map_err(|e| CliError::Internal(e.to_string()))
}

pub fn render_surfaces(format: OutputFormat, v: &SurfacesView) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(v),
        OutputFormat::Human => {
            println!("{}", style::section("SESSION"));
            kv("SESSION", style::strong(v.session));
            block("CHANGED FILES", &v.changed_files);
            let modified: Vec<String> = v
                .modified_symbols
                .iter()
                .map(|m| {
                    format!(
                        "{}  {}",
                        m.surface,
                        style::classification(&m.classification)
                    )
                })
                .collect();
            block("MODIFIED SYMBOLS", &modified);
            block("EXTERNAL CRATES", &v.external_crates);
            block("FALLBACK (working copy did not parse)", &v.fallback_files);
            block("NOT ANALYZED", &v.unparsed_files);
            println!();
            kv("REFERENCES", style::dim(v.references.len()));
            Ok(())
        }
    }
}

/// The reconciliation report (§16 step 13): the candidate, what converged,
/// what is contested and whose version it keeps, what blocks promotion, the
/// gates, and whether the session's retained code is compatible (§52).
pub fn render_candidate(
    format: OutputFormat,
    session: &doubledoor_core::DoorSession,
    c: &doubledoor_reconcile::Candidate,
    v: Option<&crate::reconcile::ValidationRecord>,
    e: Option<&doubledoor_core::evidence::EvidenceBundle>,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(&serde_json::json!({
            "session": session.id,
            "state": session.state,
            "candidate": c,
            "validation": v.map(|v| &v.report),
            "compatibility": v.map(|v| &v.compatibility),
            "evidence": e,
        })),
        OutputFormat::Human => {
            println!("{}", style::section("CANDIDATE"));
            kv("SESSION", style::strong(session.id));
            kv("STATE", style::state(session.state.to_string()));
            kv("CANDIDATE", style::strong(c.id));
            kv("MODE", style::dim(format!("{:?}", c.mode).to_lowercase()));
            kv("COMMIT", style::dim(&c.commit));
            kv("TARGET", style::dim(&c.target));
            kv(
                "STATUS",
                style::state(format!("{:?}", c.status).to_uppercase()),
            );
            kv(
                "PROMOTABLE",
                if c.promotable {
                    style::good("true")
                } else {
                    style::caution("false")
                },
            );
            let files: Vec<String> = c
                .files
                .iter()
                .map(|f| {
                    format!(
                        "{}  {}  {}",
                        f.path,
                        style::dim(format!("{:?}", f.outcome).to_lowercase()),
                        style::dim(format!("{:?}", f.method).to_lowercase())
                    )
                })
                .collect();
            block("FILES", &files);
            let contested: Vec<String> = c
                .contested
                .iter()
                .map(|x| {
                    let res = match &x.resolution {
                        doubledoor_reconcile::candidate::Resolution::Resolved => {
                            style::good("resolved")
                        }
                        doubledoor_reconcile::candidate::Resolution::Unresolved { ghosts } => {
                            style::caution(format!(
                                "unresolved ({})",
                                ghosts
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                        }
                    };
                    let kept = format!("{:?}", x.kept).to_lowercase();
                    format!(
                        "{}  {}  {res}",
                        x.surface,
                        if kept == "session" {
                            style::good("kept=session")
                        } else {
                            style::info("kept=canonical")
                        }
                    )
                })
                .collect();
            block("CONTESTED (isolated, never blended)", &contested);
            let deps: Vec<String> = c
                .graph
                .edges
                .iter()
                .map(|e| {
                    format!(
                        "{} {} {}",
                        style::dim(&e.from),
                        style::dim("->"),
                        if e.breaking {
                            style::caution(format!("{}  (breaking)", e.to))
                        } else {
                            e.to.to_string()
                        }
                    )
                })
                .collect();
            block("DEPENDS ON CANONICAL CHANGES", &deps);
            let blocking: Vec<String> = c
                .blocking
                .iter()
                .map(|b| format!("{} {}", style::marker_failed(), b))
                .collect();
            block("BLOCKING PROMOTION", &blocking);
            if let Some(v) = v {
                let gates: Vec<String> = v
                    .report
                    .gates
                    .iter()
                    .map(|g| {
                        let st = serde_json::to_value(&g.status)
                            .ok()
                            .and_then(|x| x["status"].as_str().map(ToString::to_string))
                            .unwrap_or_default();
                        format!(
                            "{}  {}  {}",
                            style::strong(&g.name),
                            style::gate_status(&st),
                            style::dim(format!("{}ms", g.duration_ms))
                        )
                    })
                    .collect();
                block("VALIDATION", &gates);
                let compat: Vec<String> = v
                    .compatibility
                    .iter()
                    .map(|r| {
                        let status = match r.status.as_str() {
                            "compatible" => style::good("compatible"),
                            "incompatible" => style::bad("incompatible"),
                            _ => style::accent(r.status.clone()),
                        };
                        let mut line = format!("{}  {}", style::strong(&r.surface), status);
                        for d in &r.diagnostics {
                            line.push_str(&format!("\n{}", style::sub_item(d)));
                        }
                        line
                    })
                    .collect();
                block("RETAINED SESSION CODE (compatibility)", &compat);
                println!();
                kv(
                    "VALIDATED",
                    if v.report.passed {
                        style::good("true")
                    } else {
                        style::bad("false")
                    },
                );
            }
            if let Some(e) = e {
                render_evidence_human(e);
            }
            Ok(())
        }
    }
}

pub fn render_promotion(
    format: OutputFormat,
    session: &doubledoor_core::DoorSession,
    c: &doubledoor_reconcile::Candidate,
    head: &doubledoor_core::GitOid,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => print_json(&serde_json::json!({
            "session": session.id,
            "state": session.state,
            "candidate": c.id,
            "from": c.target,
            "to": head,
            "partial": c.is_partial(),
            "isolated": c.contested.iter().map(|x| &x.surface).collect::<Vec<_>>(),
        })),
        OutputFormat::Human => {
            println!("{}", style::section("PROMOTION"));
            kv("SESSION", style::strong(session.id));
            kv("STATE", style::state(session.state.to_string()));
            kv("PROMOTED", style::good(c.id));
            kv("CANONICAL", style::dim(format!("{} -> {head}", c.target)));
            kv(
                "PARTIAL",
                if c.is_partial() {
                    style::caution("true")
                } else {
                    style::good("false")
                },
            );
            if c.is_partial() {
                let isolated: Vec<String> = c
                    .contested
                    .iter()
                    .map(|x| style::accent(&x.surface))
                    .collect();
                block("STILL ISOLATED IN THE SESSION", &isolated);
            }
            Ok(())
        }
    }
}

/// The unified evidence section (§49): verdict over deterministic evidence,
/// coverage of the five required kinds, provider runs, disagreements, the
/// actor's claim kept apart, and advisory (heuristic/model) notes.
fn render_evidence_human(e: &doubledoor_core::evidence::EvidenceBundle) {
    use doubledoor_core::evidence::{ProviderStatus, Verdict};
    println!();
    kv(
        "EVIDENCE",
        style::verdict(format!("{:?}", e.assessment.verdict).to_uppercase()),
    );
    let cov: Vec<String> = e
        .assessment
        .coverage
        .iter()
        .map(|(k, n)| {
            format!(
                "{}  {}",
                style::dim(k),
                style::strong(format!("{n} deterministic check(s)"))
            )
        })
        .collect();
    block("COVERAGE", &cov);
    let providers: Vec<String> = e
        .providers
        .iter()
        .map(|p| {
            let st = match &p.status {
                ProviderStatus::Ran { items } => style::good(format!("ran ({items} item(s))")),
                ProviderStatus::Unavailable { reason } => {
                    style::dim(format!("unavailable: {reason}"))
                }
                ProviderStatus::Failed { reason } => style::bad(format!("failed: {reason}")),
                ProviderStatus::Disabled => style::dim("disabled"),
            };
            let tier = p.tier.map(snake).unwrap_or_else(|| "sink".to_string());
            format!(
                "{}  {}  {}",
                style::strong(&p.provider),
                style::dim(format!("[{tier}]")),
                st
            )
        })
        .collect();
    block("PROVIDERS", &providers);
    let findings: Vec<String> = e
        .items
        .iter()
        .filter(|i| i.tier.is_deterministic() && i.verdict >= Verdict::Concern)
        .map(|i| {
            let loc = i
                .file
                .as_ref()
                .map(|f| {
                    format!(
                        "  {}",
                        style::dim(format!(
                            "{f}{}",
                            i.line.map(|l| format!(":{l}")).unwrap_or_default()
                        ))
                    )
                })
                .unwrap_or_default();
            format!(
                "{}  {}  {}{loc}",
                style::verdict(snake(i.verdict)),
                i.summary,
                style::dim(format!("[{} · {}]", i.provider, snake(i.tier)))
            )
        })
        .collect();
    block("FINDINGS (deterministic)", &findings);
    let conflicts: Vec<String> = e
        .conflicts
        .iter()
        .map(|c| format!("{} {}", style::caution("!"), c.note))
        .collect();
    block("PROVIDERS DISAGREE", &conflicts);
    let mut claim = Vec::new();
    if let Some(c) = &e.explanation.actor_claim {
        claim.push(format!("{} {}", style::accent("actor claims:"), c));
    }
    if let Some(i) = &e.explanation.inferred_intent {
        claim.push(format!("{} {}", style::accent("inferred:"), i));
    }
    claim.push(style::dim(format!(
        "{} deterministic / {} probabilistic evidence item(s)",
        e.explanation.deterministic_evidence.len(),
        e.explanation.probabilistic_evidence.len()
    )));
    block("CLAIMS VS EVIDENCE (§18)", &claim);
    let advisory: Vec<String> = e
        .assessment
        .advisory
        .iter()
        .map(|a| style::dim(a.clone()))
        .collect();
    block("ADVISORY (heuristic/model; never decisive)", &advisory);
}

/// The serde (snake_case) name of an enum value.
fn snake(v: impl Serialize) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|x| x.as_str().map(ToString::to_string))
        .unwrap_or_default()
}
