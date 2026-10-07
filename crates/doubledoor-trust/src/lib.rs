//! Trust primitives for distributed Doubledoor (§51 Phase VII): OpenSSH
//! signatures (`ssh-keygen -Y sign|verify`), namespaced so a signature made
//! for one purpose can never be replayed for another.
//!
//! | namespace | signs | trusted signers |
//! |---|---|---|
//! | [`NS_LEASE`] `locksmith-lease` | Locksmith lease grants (item 3) | `trust/locksmith.allowed_signers` |
//! | [`NS_ACTOR`] `doubledoor-actor` | actor authentication challenges (item 5) | `trust/actors.allowed_signers` |
//! | [`NS_PROVENANCE`] `doubledoor-provenance` | mutation provenance attestations (item 6) | `trust/provenance.allowed_signers` |
//!
//! Standard formats only: OpenSSH `allowed_signers` files and armored SSH
//! signatures, so keys already used for SSH/git signing work unchanged, and
//! verification needs nothing but `ssh-keygen`. The trust store lives in
//! `.doubledoor/trust/`; it holds public material only (private keys are
//! referenced by path, never copied).

#![forbid(unsafe_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const NS_LEASE: &str = "locksmith-lease";
pub const NS_ACTOR: &str = "doubledoor-actor";
pub const NS_PROVENANCE: &str = "doubledoor-provenance";

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("trust I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ssh-keygen is not available: {0}")]
    NoSshKeygen(String),
    #[error("signing failed: {0}")]
    Sign(String),
    /// The signature does not verify for that principal and namespace.
    #[error("signature rejected: {0}")]
    Rejected(String),
}

/// Who a verified signature belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verified {
    pub principal: String,
    pub namespace: String,
    /// `SHA256:…` key fingerprint.
    pub fingerprint: String,
}

/// Canonical JSON: keys sorted, no insignificant whitespace, UTF-8 as is.
/// What every signer and verifier must hash.
pub fn canonical_json(v: &serde_json::Value) -> Vec<u8> {
    // serde_json's default map is ordered by key, so this is sorted.
    serde_json::to_vec(v).unwrap_or_default()
}

fn scratch_file(dir: &Path, stem: &str, data: &[u8]) -> Result<PathBuf, TrustError> {
    fs::create_dir_all(dir)?;
    let p = dir.join(format!(
        "{stem}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::write(&p, data)?;
    Ok(p)
}

/// Sign `data` with the private key at `key` for `namespace`; returns the
/// armored signature.
pub fn sign(
    key: &Path,
    namespace: &str,
    data: &[u8],
    scratch: &Path,
) -> Result<String, TrustError> {
    let file = scratch_file(scratch, "sign", data)?;
    let out = Command::new("ssh-keygen")
        .args(["-q", "-Y", "sign", "-n", namespace, "-f"])
        .arg(key)
        .arg(&file)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| TrustError::NoSshKeygen(e.to_string()));
    let sig_path = PathBuf::from(format!("{}.sig", file.display()));
    let result = match out {
        Ok(o) if o.status.success() => fs::read_to_string(&sig_path).map_err(TrustError::from),
        Ok(o) => Err(TrustError::Sign(
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        )),
        Err(e) => Err(e),
    };
    let _ = fs::remove_file(&file);
    let _ = fs::remove_file(&sig_path);
    result
}

/// Verify an armored `signature` over `data` as `principal` in `namespace`
/// against an `allowed_signers` file.
pub fn verify(
    allowed_signers: &Path,
    principal: &str,
    namespace: &str,
    data: &[u8],
    signature: &str,
    scratch: &Path,
) -> Result<Verified, TrustError> {
    if !allowed_signers.is_file() {
        return Err(TrustError::Rejected(format!(
            "no trust store at {}",
            allowed_signers.display()
        )));
    }
    let sig = scratch_file(scratch, "verify-sig", signature.as_bytes())?;
    let child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-n", namespace, "-I", principal, "-f"])
        .arg(allowed_signers)
        .arg("-s")
        .arg(&sig)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let result = (|| {
        let mut child = child.map_err(|e| TrustError::NoSshKeygen(e.to_string()))?;
        child
            .stdin
            .take()
            .ok_or_else(|| TrustError::NoSshKeygen("no stdin".into()))?
            .write_all(data)?;
        let out = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            return Err(TrustError::Rejected(
                format!(
                    "{} {}",
                    stdout.trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )
                .trim()
                .to_string(),
            ));
        }
        // `Good "<ns>" signature for <principal> with <TYPE> key SHA256:…`
        let fingerprint = stdout
            .split_whitespace()
            .find(|t| t.starts_with("SHA256:"))
            .unwrap_or_default()
            .to_string();
        Ok(Verified {
            principal: principal.to_string(),
            namespace: namespace.to_string(),
            fingerprint,
        })
    })();
    let _ = fs::remove_file(&sig);
    result
}

/// `.doubledoor/trust/`.
#[derive(Debug, Clone)]
pub struct TrustStore {
    pub dir: PathBuf,
}

/// `trust/signing.json`: this host's signing identity for provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningConfig {
    /// Path to the private key (never copied into the store).
    pub key: PathBuf,
    /// The principal it signs as.
    pub principal: String,
}

impl TrustStore {
    pub fn new(store_root: &Path) -> Self {
        TrustStore {
            dir: store_root.join("trust"),
        }
    }

    pub fn locksmith_signers(&self) -> PathBuf {
        self.dir.join("locksmith.allowed_signers")
    }

    pub fn actor_signers(&self) -> PathBuf {
        self.dir.join("actors.allowed_signers")
    }

    pub fn provenance_signers(&self) -> PathBuf {
        self.dir.join("provenance.allowed_signers")
    }

    pub fn scratch(&self) -> PathBuf {
        self.dir.join(".scratch")
    }

    /// Leases must be signed once a Locksmith key is trusted.
    pub fn requires_signed_leases(&self) -> bool {
        self.locksmith_signers().is_file()
    }

    /// Mutating protocol calls must be authenticated once actor keys are
    /// trusted.
    pub fn requires_actor_auth(&self) -> bool {
        self.actor_signers().is_file()
    }

    pub fn signing(&self) -> Option<SigningConfig> {
        fs::read_to_string(self.dir.join("signing.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// This host's identity: `trust/host.json` `{"host_id": …}`, else the
    /// system hostname.
    pub fn host_id(&self) -> String {
        fs::read_to_string(self.dir.join("host.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v["host_id"].as_str().map(ToString::to_string))
            .or_else(|| {
                fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|h| h.trim().to_string())
            })
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "localhost".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair(dir: &Path, name: &str) -> PathBuf {
        let k = dir.join(name);
        let st = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
            .arg(&k)
            .status()
            .unwrap();
        assert!(st.success());
        k
    }

    fn allow(dir: &Path, principal: &str, key: &Path) -> PathBuf {
        let p = dir.join("allowed");
        let pubkey = fs::read_to_string(format!("{}.pub", key.display())).unwrap();
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .unwrap();
        writeln!(f, "{principal} {}", pubkey.trim()).unwrap();
        p
    }

    #[test]
    fn sign_verify_and_every_way_to_fail() {
        let d = tempfile::tempdir().unwrap();
        let scratch = d.path().join("s");
        let k = keypair(d.path(), "locksmith");
        let other = keypair(d.path(), "mallory");
        let allowed = allow(d.path(), "locksmith", &k);
        let data = canonical_json(&serde_json::json!({"b": 1, "a": [2, 3]}));
        assert_eq!(data, br#"{"a":[2,3],"b":1}"#, "sorted, compact");
        let sig = sign(&k, NS_LEASE, &data, &scratch).unwrap();
        let v = verify(&allowed, "locksmith", NS_LEASE, &data, &sig, &scratch).unwrap();
        assert!(v.fingerprint.starts_with("SHA256:"));

        // Tampered data, wrong namespace, wrong principal, untrusted key.
        assert!(verify(&allowed, "locksmith", NS_LEASE, b"{}", &sig, &scratch).is_err());
        assert!(verify(&allowed, "locksmith", NS_PROVENANCE, &data, &sig, &scratch).is_err());
        assert!(verify(&allowed, "someone", NS_LEASE, &data, &sig, &scratch).is_err());
        let forged = sign(&other, NS_LEASE, &data, &scratch).unwrap();
        assert!(verify(&allowed, "locksmith", NS_LEASE, &data, &forged, &scratch).is_err());
        assert!(verify(&allowed, "locksmith", NS_LEASE, &data, "garbage", &scratch).is_err());
        assert!(
            fs::read_dir(&scratch).unwrap().next().is_none(),
            "scratch files are cleaned up"
        );
    }
}
