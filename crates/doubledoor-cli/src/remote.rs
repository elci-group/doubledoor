//! Phase VII commands (§51): remotes, and sessions hosted on them.
//!
//! Every distributed operation first requires clean local recovery on this
//! host (§51: "Distributed operation MUST NOT precede robust local recovery
//! semantics") and, where it touches a remote, on the remote too.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use doubledoor_core::ActorId;
use doubledoor_remote::config::valid_name;
use doubledoor_remote::{Client, Remote, RemoteError, RemotesConfig};

use crate::commands::{load_notice, require_local_readiness, resolve_git, store_root};
use crate::error::CliError;
use crate::{Cli, RemoteCmd};

/// Protocol round-trip budget for remote calls (reconcile can be long).
const REMOTE_TIMEOUT: Duration = Duration::from_secs(1800);

pub(crate) fn load(git: &doubledoor_git::Git) -> Result<RemotesConfig, CliError> {
    RemotesConfig::load(&store_root(git)).map_err(CliError::CorruptMetadata)
}

pub(crate) fn remote<'c>(cfg: &'c RemotesConfig, name: &str) -> Result<&'c Remote, CliError> {
    cfg.remotes
        .get(name)
        .ok_or_else(|| RemoteError::Unknown(name.to_string()).into())
}

/// Connect and require the remote to be recovery-clean too.
pub(crate) fn connect_ready(r: &Remote, caps: &[&str]) -> Result<Client, CliError> {
    let mut c = Client::connect(r, caps, REMOTE_TIMEOUT)?;
    let doctor = c.call("doctor", json!({}))?;
    let ready = doctor["checks"]
        .as_array()
        .and_then(|cs| cs.iter().find(|c| c["name"] == "local-recovery-ready"))
        .cloned();
    match ready {
        Some(check) if check["ok"] == true => Ok(c),
        Some(check) => Err(CliError::InvalidTransition(format!(
            "remote is not recovery-clean (§51): {}",
            check["detail"].as_str().unwrap_or_default()
        ))),
        None => Err(CliError::InvalidTransition(
            "remote Doubledoor predates Phase VII readiness checks".to_string(),
        )),
    }
}

fn links_dir(git: &doubledoor_git::Git) -> PathBuf {
    store_root(git).join("remote-sessions")
}

pub fn dispatch(cli: &Cli, cmd: &RemoteCmd) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let store = store_root(&git);
    match cmd {
        RemoteCmd::Add {
            name,
            path,
            via,
            doubledoor,
        } => {
            if !valid_name(name) {
                return Err(CliError::Usage(format!(
                    "remote name `{name}` must be [A-Za-z0-9_-]+"
                )));
            }
            let r = Remote {
                transport: via.clone(),
                doubledoor: doubledoor.clone(),
                repo: path.clone(),
                node: None,
                identity: None,
            };
            r.validate().map_err(CliError::Usage)?;
            let mut cfg = load(&git)?;
            let existing = cfg.remotes.get(name).cloned();
            let r = Remote {
                node: existing.as_ref().and_then(|e| e.node.clone()),
                identity: existing.and_then(|e| e.identity),
                ..r
            };
            cfg.remotes.insert(name.clone(), r);
            crate::commands::ensure_store(&git)?;
            cfg.save(&store).map_err(CliError::Internal)?;
            crate::output::print_json(&json!({ "added": name, "remote": cfg.remotes[name] }))
        }
        RemoteCmd::List => crate::output::print_json(&load(&git)?),
        RemoteCmd::Remove { name } => {
            let mut cfg = load(&git)?;
            if cfg.remotes.remove(name).is_none() {
                return Err(RemoteError::Unknown(name.clone()).into());
            }
            cfg.save(&store).map_err(CliError::Internal)?;
            crate::output::print_json(&json!({ "removed": name }))
        }
        RemoteCmd::Probe { name, agent } => {
            let mut cfg = load(&git)?;
            let r = remote(&cfg, name)?.clone();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let facts = doubledoor_remote::node::probe(&r, agent, now)
                .map_err(|e| CliError::Internal(format!("probe of `{name}` failed: {e}")))?;
            let v = serde_json::to_value(&facts).unwrap_or(Value::Null);
            if let Some(entry) = cfg.remotes.get_mut(name) {
                entry.node = Some(v.clone());
            }
            cfg.save(&store).map_err(CliError::Internal)?;
            crate::output::print_json(&json!({ "remote": name, "node": v }))
        }
        RemoteCmd::Ping { name } => {
            let cfg = load(&git)?;
            let r = remote(&cfg, name)?;
            let mut c = Client::connect(r, &["sessions"], REMOTE_TIMEOUT)?;
            let status = c.call("status", json!({}))?;
            let doctor = c.call("doctor", json!({}))?;
            let ready = doctor["checks"]
                .as_array()
                .and_then(|cs| cs.iter().find(|c| c["name"] == "local-recovery-ready"))
                .map(|c| c["ok"] == true)
                .unwrap_or(false);
            let local = git.head_oid()?;
            let head = status["canonical_head"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            crate::output::print_json(&json!({
                "remote": name,
                "protocol": c.hello["protocol"],
                "server": c.hello["server"],
                "canonical_head": head,
                "local_head": local,
                "in_sync": head == local.as_str(),
                "recovery_ready": ready,
                "sessions": status["total"],
            }))
        }
        RemoteCmd::Exec {
            name,
            method,
            params,
        } => {
            let params: Value = serde_json::from_str(params)
                .map_err(|e| CliError::Usage(format!("--params must be a JSON object: {e}")))?;
            let cfg = load(&git)?;
            let r = remote(&cfg, name)?;
            require_local_readiness(&git)?;
            let mut c = Client::connect(r, &[], REMOTE_TIMEOUT)?;
            let result = c.call(method, params)?;
            crate::output::print_json(&result)
        }
        RemoteCmd::Sessions => {
            let cfg = load(&git)?;
            let mut out = Vec::new();
            for link in read_links(&git) {
                let name = link["remote"].as_str().unwrap_or_default().to_string();
                let id = link["session"].as_str().unwrap_or_default().to_string();
                let live = match cfg
                    .remotes
                    .get(&name)
                    .map(|r| Client::connect(r, &[], Duration::from_secs(60)))
                {
                    Some(Ok(mut c)) => c
                        .call("inspect", json!({ "session": id }))
                        .unwrap_or_else(|e| json!({ "error": e.to_string() })),
                    Some(Err(e)) => json!({ "error": e.to_string() }),
                    None => json!({ "error": "remote no longer registered" }),
                };
                out.push(json!({ "link": link, "live": live }));
            }
            crate::output::print_json(&out)
        }
    }
}

fn read_links(git: &doubledoor_git::Git) -> Vec<Value> {
    let mut links: Vec<Value> = fs::read_dir(links_dir(git))
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| fs::read_to_string(e.path()).ok())
                .filter_map(|s| serde_json::from_str(&s).ok())
                .collect()
        })
        .unwrap_or_default();
    links.sort_by_key(|l| l["opened_at_millis"].as_u64().unwrap_or(0));
    links
}

/// `open --remote <name>`: host a DoorSession on a remote Doubledoor.
#[allow(clippy::too_many_arguments)]
pub fn open_remote(
    cli: &Cli,
    name: &str,
    actor: &str,
    intent: &str,
    surfaces: &[String],
    sets: (&Vec<String>, &Vec<String>, &Vec<String>),
    notice_path: Option<&Path>,
    auto_accept: &str,
    require_harness: &[String],
) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    require_local_readiness(&git)?;
    let actor_id = ActorId::new(actor).map_err(|e| CliError::Usage(e.to_string()))?;
    // The same Locksmith authority check as a local open (DD-I6).
    let notice = load_notice(notice_path, &actor_id)?;
    let cfg = load(&git)?;
    let placement = crate::placement::resolve(&git, &cfg, name, require_harness)?;
    let name = placement.chosen.clone();
    let r = remote(&cfg, &name)?;
    let mut c = connect_ready(r, &["sessions"])?;

    // The remote must host the same canonical state we speculate from.
    let local = git.head_oid()?;
    let status = c.call("status", json!({}))?;
    let remote_head = status["canonical_head"].as_str().unwrap_or_default();
    if remote_head != local.as_str() {
        return Err(CliError::InvalidTransition(format!(
            "remote `{name}` canonical is {remote_head}, local is {local}; run `doubledoor remote sync {name}` first"
        )));
    }
    let (read, write, depends) = sets;
    let summary = c.call(
        "open",
        json!({
            "actor": actor,
            "intent": intent,
            "surfaces": surfaces,
            "read": read,
            "write": write,
            "depends": depends,
            "notice": notice,
            "auto_accept": auto_accept,
        }),
    )?;
    let id = summary["id"].as_str().unwrap_or_default().to_string();
    let dir = links_dir(&git);
    fs::create_dir_all(&dir).map_err(|e| CliError::Internal(e.to_string()))?;
    let link = json!({
        "remote": name,
        "session": id,
        "actor": actor,
        "intent": intent,
        "base_commit": local,
        "opened_at_millis": SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
    });
    let file = dir.join(format!("{id}.json"));
    fs::write(&file, serde_json::to_vec_pretty(&link).unwrap_or_default())
        .map_err(|e| CliError::Internal(e.to_string()))?;
    let mut out = summary;
    out["remote"] = json!(name);
    out["placement"] = serde_json::to_value(&placement).unwrap_or(Value::Null);
    crate::output::print_json(&out)
}
