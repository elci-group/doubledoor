//! `doubledoor` — agent-aware speculative concurrency for Git repositories.
//!
//! This binary is a thin orchestration layer over the Doubledoor library
//! crates; all domain logic (state machine, policy, journal, Git isolation)
//! lives there. Implemented in Phase I: `status`, `sessions`, `open`,
//! `inspect`, `abandon`, `recover`, `doctor`. Implemented in Phase II (ghost
//! observation): `watch`, `events`, `ghosts`, `accept`, `defer`, `reject`.
//! Phase III (semantic surfaces) adds `surfaces` and Rust symbol / Cargo
//! assessment of every ghost. Phase IV (reconciliation) implements
//! `reconcile`, `validate`, `promote`, and adds `candidate`, `discard`, and
//! `accept --symbol`. Phase V attaches unified evidence to every candidate.
//! Phase VI (agent-native operation) adds the `doubledoor.protocol/v1`
//! server (`serve`, `rpc`), `capabilities`, `report`, `policy`, automatic
//! low-risk acceptance (`open --auto-accept`), and JSON errors under
//! `--format json`.

#![forbid(unsafe_code)]

mod agent;
mod commands;
mod error;
mod ghosts;
mod output;
mod placement;
mod reconcile;
mod remote;
mod style;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

pub use error::CliError;
pub use output::OutputFormat;

const EXIT_CODES_HELP: &str = "\
EXIT CODES (stable contract, §30)
    0  ok
    1  usage error (bad flags, or no git repository resolvable)
    2  policy denied the session-open request
    3  request contends but no valid contention notice was supplied
    4  workspace isolation failed (session is FAILED_ISOLATION)
    5  requested session not found
    6  requested state transition is not legal from the current state
    7  session metadata or journal is corrupt or schema-incompatible
    8  internal error
    9  subcommand is declared for a later phase (not implemented)
   10  a change cannot be applied cleanly (uncommitted edits in the way, or
       the delta does not apply); nothing was changed
   11  the Locksmith lease does not permit the canonical mutation
   12  canonical advanced past the candidate (stale; nothing was changed)

`doctor` exits 0 when every check passes and 1 when any check fails; that is
the diagnostic meaning of exit 1 for that command only.";

#[derive(Parser)]
#[command(
    name = "doubledoor",
    about = "Agent-aware speculative concurrency for Git repositories (Phases I-VI: isolation, ghost observation, semantic surfaces, reconciliation, evidence, agent protocol)",
    after_long_help = EXIT_CODES_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Print version information (legal before or after any subcommand)
    #[arg(short = 'v', long = "version", global = true, action = clap::ArgAction::Version)]
    version: (),

    /// Repository root (default: discover the enclosing git repository upward
    /// from the current directory)
    #[arg(long, global = true, value_name = "PATH")]
    repo: Option<PathBuf>,

    /// Output format
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,

    /// Verbose tracing output on stderr
    #[arg(long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Aggregate status of every DoorSession in this repository
    Status,
    /// List every DoorSession (id, state, actor, intent)
    Sessions,
    /// Open a new DoorSession: policy check, then an isolated detached
    /// worktree for speculative mutation
    Open {
        /// Stable actor identifier
        #[arg(long, value_name = "ID")]
        actor: String,
        /// What the actor intends to do, in their own words
        #[arg(long, value_name = "TEXT")]
        intent: String,
        /// Mutation surface the actor requests (repeatable). Surfaces that
        /// name a tracked file also join the session's write set
        #[arg(long = "surface", value_name = "SURFACE", required = true)]
        surfaces: Vec<String>,
        /// Repo-relative path the actor reads (§11 read set; repeatable)
        #[arg(long = "read", value_name = "PATH")]
        read: Vec<String>,
        /// Repo-relative path the actor will modify (§11 write set; repeatable)
        #[arg(long = "write", value_name = "PATH")]
        write: Vec<String>,
        /// Repo-relative path the actor's work depends on (§11 dependency
        /// set; repeatable)
        #[arg(long = "depends", value_name = "PATH")]
        depends: Vec<String>,
        /// Locksmith contention notice (JSON file) authorizing speculation
        #[arg(long, value_name = "JSON-FILE")]
        notice: Option<PathBuf>,
        /// Automatic low-risk acceptance: off, independent,
        /// dependency_relevant (explicit opt-in; journalled)
        #[arg(long = "auto-accept", value_name = "LEVEL", default_value = "off")]
        auto_accept: String,
        /// Host the session on a registered remote (Phase VII); `auto`
        /// places it on the best-suited probed Dream VPS node
        #[arg(long, value_name = "NAME")]
        remote: Option<String>,
        /// With `--remote auto`: the node must report this harness
        /// (repeatable)
        #[arg(long = "require-harness", value_name = "NAME")]
        require_harness: Vec<String>,
    },
    /// Full detail for one session: state, base, journal size, workspace
    Inspect {
        /// Session id (`dd_<uuid>`)
        id: String,
    },
    /// Abandon a live session: remove its workspace, mark ABANDONED
    Abandon {
        /// Session id (`dd_<uuid>`)
        id: String,
    },
    /// Replay every session journal and reconcile metadata with on-disk
    /// reality after a crash (canonical refs are never modified)
    Recover,
    /// Diagnose the environment: git availability, repo discovery, schema
    /// compatibility, journal readability, worktree consistency.
    /// Exits 0 when all checks pass, 1 when any check fails
    Doctor,
    /// Canonical commit watcher: observe new canonical commits for every
    /// live session, record them as ghosts, and publish events. Workspaces
    /// are never altered
    Watch {
        /// Poll interval for canonical HEAD, in milliseconds
        #[arg(long, value_name = "MS", default_value_t = 1000)]
        interval_ms: u64,
        /// Run a single observation pass and exit
        #[arg(long)]
        once: bool,
    },
    /// Read the local event stream (ghost.available, ghost.assessed,
    /// session.state) as JSON lines
    Events {
        /// Only events with a sequence number greater than this
        #[arg(long, value_name = "SEQ", default_value_t = 0)]
        since: u64,
        /// Keep streaming new events as they are written
        #[arg(long)]
        follow: bool,
        /// Only events for this session
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Stop after this many events
        #[arg(long, value_name = "N")]
        max: Option<usize>,
    },
    /// Observe, then list a session's ghosts with their status
    Ghosts {
        /// Session id (`dd_<uuid>`)
        session: String,
        /// List recorded ghosts without observing new canonical commits
        #[arg(long)]
        no_observe: bool,
    },
    /// Show a session's derived surfaces: files it changed, Rust symbols it
    /// modified, what its changed code references, and the crates it uses
    Surfaces {
        /// Session id (`dd_<uuid>`)
        session: String,
    },
    /// Accept a ghost: import its changes into the session workspace as a
    /// new commit (optionally only the named surfaces, §15)
    Accept {
        /// Ghost id (`gh_<uuid>`)
        ghost: String,
        /// Import only this file (path or `file:<path>`; repeatable)
        #[arg(long = "surface", value_name = "SURFACE")]
        surfaces: Vec<String>,
        /// Import only this Rust item (`rust:<path>` from the ghost's
        /// assessment; repeatable). Other items in the same file, including
        /// the session's own edits, are kept
        #[arg(long = "symbol", value_name = "SYMBOL")]
        symbols: Vec<String>,
    },
    /// Defer a ghost: acknowledge it and continue on the current state; it
    /// stays pending and can be accepted or rejected later
    Defer {
        /// Ghost id (`gh_<uuid>`)
        ghost: String,
    },
    /// Reject a ghost for this session (canonical history is untouched)
    Reject {
        /// Ghost id (`gh_<uuid>`)
        ghost: String,
    },
    /// Build a reconciliation candidate on top of canonical HEAD — the
    /// session's work merged with canonical changes (independent parts
    /// converge; contested parts are never blended) — and validate it
    Reconcile {
        /// Session id (`dd_<uuid>`)
        session: String,
        /// Contested surfaces keep canonical's version, so the session's
        /// uncontested work can be promoted while the contested work stays
        /// isolated in the session
        #[arg(long)]
        converge: bool,
    },
    /// Re-run the validation gates for the session's current candidate
    Validate {
        /// Session id (`dd_<uuid>`)
        session: String,
    },
    /// Show the session's latest candidate, conflict graph, and validation
    Candidate {
        /// Session id (`dd_<uuid>`)
        session: String,
    },
    /// Machine-readable session report (`doubledoor.report/v1`) with the
    /// next actions as callable protocol methods
    Report {
        /// Session id (`dd_<uuid>`)
        session: String,
    },
    /// Set a session's automatic low-risk acceptance level, then apply it
    Policy {
        /// Session id (`dd_<uuid>`)
        session: String,
        /// off, independent, dependency_relevant
        #[arg(long = "auto-accept", value_name = "LEVEL")]
        auto_accept: String,
    },
    /// Remote Doubledoors hosting sessions on other hosts (Phase VII)
    Remote {
        #[command(subcommand)]
        cmd: RemoteCmd,
    },
    /// What a protocol `hello` grants: methods, schemas, capabilities
    Capabilities,
    /// Speak `doubledoor.protocol/v1` on stdin/stdout (one JSON per line)
    Rpc,
    /// Serve `doubledoor.protocol/v1` on a Unix socket, with a watcher
    Serve {
        /// Socket path (default: .doubledoor/doubledoor.sock)
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
        /// Watcher poll interval, in milliseconds
        #[arg(long, value_name = "MS", default_value_t = 1000)]
        watch_interval_ms: u64,
        /// Do not run the integrated watcher
        #[arg(long)]
        no_watch: bool,
    },
    /// Set the current candidate aside and resume speculative work
    Discard {
        /// Session id (`dd_<uuid>`)
        session: String,
    },
    /// Promote a READY candidate onto canonical: compare-and-swap against
    /// the candidate's target, authorized by a Locksmith lease
    Promote {
        /// Session id (`dd_<uuid>`)
        session: String,
        /// Locksmith lease grant (JSON file) covering every changed file
        #[arg(long, value_name = "JSON-FILE")]
        lease: PathBuf,
    },
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Register a remote Doubledoor serving a clone of this canonical repo
    Add {
        name: String,
        /// The clone's path on the remote host (named `--path`: `--repo`
        /// is the global flag selecting this repository)
        #[arg(long = "path", value_name = "PATH")]
        path: String,
        /// Transport argv element (repeatable), e.g. `--via ssh --via
        /// -oBatchMode=yes --via host`; none runs on this host
        #[arg(long = "via", value_name = "ARG", allow_hyphen_values = true)]
        via: Vec<String>,
        /// The doubledoor binary on the remote
        #[arg(long, value_name = "BIN", default_value = "doubledoor")]
        doubledoor: String,
    },
    /// List registered remotes
    List,
    /// Forget a remote
    Remove { name: String },
    /// Probe a Dream VPS node: its capability report (`dreamseq-node
    /// capabilities`) and Doubledoor's presence, recorded for placement
    Probe {
        name: String,
        /// The node agent binary on the remote
        #[arg(long, value_name = "BIN", default_value = "dreamseq-node")]
        agent: String,
    },
    /// Connect, negotiate, and report the remote's canonical head and
    /// local-recovery readiness
    Ping { name: String },
    /// Call one protocol method on a remote (params as a JSON object)
    Exec {
        name: String,
        method: String,
        #[arg(long, value_name = "JSON", default_value = "{}")]
        params: String,
    },
    /// Sessions this repository opened on remotes, with live state
    Sessions,
}

fn main() -> ExitCode {
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(err) => {
            // --help/--version print and exit 0; every other parse failure is
            // a usage error (exit 1), per the stable exit-code contract.
            match err.kind() {
                clap::error::ErrorKind::DisplayHelp => {
                    print!("{err}");
                    return ExitCode::from(0);
                }
                // The same line wherever `-v`/`--version` appears, never
                // clap's "doubledoor-<subcommand>" display name.
                clap::error::ErrorKind::DisplayVersion => {
                    println!("doubledoor {}", env!("CARGO_PKG_VERSION"));
                    return ExitCode::from(0);
                }
                _ => {
                    // Agents asking for JSON get a JSON usage error even when
                    // the arguments themselves could not be parsed.
                    if json_requested() {
                        emit_json_error(1, err.to_string().trim());
                    } else {
                        eprint!("{err}");
                    }
                    return ExitCode::from(1);
                }
            }
        }
    };

    if cli.verbose {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_max_level(tracing::Level::DEBUG)
            .init();
    }

    match commands::dispatch(&cli) {
        Ok(()) => ExitCode::from(0),
        Err(err) => {
            if cli.format == OutputFormat::Json {
                emit_json_error(err.exit_code(), &err.to_string());
            } else {
                eprintln!("{} {err}", style::error_prefix());
            }
            ExitCode::from(err.exit_code())
        }
    }
}

/// Parse the CLI, fixing up what derive cannot express: the version string
/// and flag wiring. The version flag is a custom global arg (`-v`, with the
/// conventional `-V` as a hidden alias), so it parses after any subcommand;
/// every (sub)command gets the version string and its auto-generated
/// `--version` disabled so only our flag answers.
fn parse_cli() -> Result<Cli, clap::Error> {
    fn prepare(cmd: clap::Command, version: &'static str) -> clap::Command {
        cmd.version(version)
            .disable_version_flag(true)
            .mut_subcommands(|sub| prepare(sub, version))
    }
    let cmd = prepare(
        Cli::command().mut_arg("version", |a| a.short_aliases(['V'])),
        env!("CARGO_PKG_VERSION"),
    );
    let matches = cmd.try_get_matches()?;
    Cli::from_arg_matches(&matches)
}

/// `--format json` / `--format=json` anywhere on the raw command line.
fn json_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    args.iter().any(|a| a == "--format=json")
        || args
            .windows(2)
            .any(|w| w[0] == "--format" && w[1] == "json")
}

/// A machine-readable error on stderr: `{"error": {code, kind, retryable,
/// message}}` (the protocol's error object, §50).
fn emit_json_error(code: u8, message: &str) {
    let err = doubledoor_protocol::rpc::ErrorObject::from_code(code, message);
    eprintln!("{}", serde_json::json!({ "error": err }));
}
