//! Padagonia sink (§21 "Padagonia — dependency and ontology
//! relationships"): publish a candidate's evidence graph — the candidate,
//! each evidence item, each subject surface, and the dependency edges —
//! into a Padagonia server as nodes and edges with provenance, so other
//! ELCI tools can traverse it. Publishing judges nothing and never affects
//! the verdict; a failure is reported and nothing else changes.
//!
//! Transport: plain HTTP/1.1 over `std::net` to a local/trusted server
//! (`http://` only), Bearer API key from an environment variable, request
//! ids and idempotency keys derived from Doubledoor ids so a re-publish is
//! idempotent.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use doubledoor_core::evidence::{EvidenceBundle, ProviderRun, ProviderStatus};

use crate::config::PadagoniaConfig;

struct Client {
    host: String,
    addr: std::net::SocketAddr,
    key: String,
    namespace: String,
}

impl Client {
    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let payload = body.to_string();
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_secs(5))
            .map_err(|e| format!("connect {}: {e}", self.addr))?;
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nx-padagonia-namespace: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            self.host,
            self.key,
            self.namespace,
            payload.len()
        );
        stream
            .write_all(req.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or("malformed HTTP response")?;
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or("malformed status line")?;
        let body = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(body)
        } else {
            body.to_string()
        };
        if !(200..300).contains(&status) {
            return Err(format!("POST {path} → {status}: {}", body.trim()));
        }
        serde_json::from_str(body.trim()).map_err(|e| format!("POST {path}: bad JSON: {e}"))
    }
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, after)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if n == 0 || after.len() < n {
            break;
        }
        out.push_str(&after[..n]);
        rest = after[n..].trim_start_matches("\r\n");
    }
    out
}

/// Publish `bundle` (plus the candidate's dependency edges). Returns the
/// provider run for the report.
pub fn publish(
    cfg: &PadagoniaConfig,
    bundle: &EvidenceBundle,
    edges: &[(String, String)],
) -> ProviderRun {
    let started = Instant::now();
    let done = |status| ProviderRun {
        provider: "padagonia".into(),
        tier: None,
        status,
        duration_ms: started.elapsed().as_millis() as u64,
        command: vec![format!("POST {}/api/v1/nodes|edges", cfg.url)],
    };
    if !cfg.enabled {
        return done(ProviderStatus::Disabled);
    }
    let Some(hostport) = cfg
        .url
        .strip_prefix("http://")
        .map(|s| s.trim_end_matches('/'))
    else {
        return done(ProviderStatus::Unavailable {
            reason: "only http:// Padagonia URLs are supported".into(),
        });
    };
    let Ok(key) = std::env::var(&cfg.api_key_env) else {
        return done(ProviderStatus::Unavailable {
            reason: format!("${} is not set", cfg.api_key_env),
        });
    };
    let Some(addr) = hostport.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return done(ProviderStatus::Unavailable {
            reason: format!("cannot resolve {hostport}"),
        });
    };
    let client = Client {
        host: hostport.to_string(),
        addr,
        key,
        namespace: cfg.namespace.clone(),
    };
    match publish_graph(&client, bundle, edges) {
        Ok(n) => done(ProviderStatus::Ran { items: n }),
        Err(reason) => done(ProviderStatus::Failed { reason }),
    }
}

fn provenance(evidence: Vec<String>) -> Value {
    json!({ "agent": "doubledoor", "model": "deterministic", "confidence": 1.0, "evidence": evidence })
}

fn publish_graph(
    c: &Client,
    b: &EvidenceBundle,
    edges: &[(String, String)],
) -> Result<usize, String> {
    let mut ids: BTreeMap<String, u64> = BTreeMap::new();
    let node = |ids: &mut BTreeMap<String, u64>,
                ext: String,
                label: &str,
                props: Value|
     -> Result<u64, String> {
        if let Some(id) = ids.get(&ext) {
            return Ok(*id);
        }
        let v = c.post(
            "/api/v1/nodes",
            &json!({
                "namespace": c.namespace,
                "external_id": ext,
                "idempotency_key": format!("doubledoor:{ext}"),
                "label": label,
                "properties": props,
                "provenance": provenance(vec![b.candidate.clone()]),
            }),
        )?;
        let id = v["id"].as_u64().ok_or("node response without id")?;
        ids.insert(ext, id);
        Ok(id)
    };
    let cand = node(
        &mut ids,
        format!("doubledoor:candidate:{}", b.candidate),
        "doubledoor_candidate",
        json!({
            "commit": b.commit, "verdict": format!("{:?}", b.assessment.verdict).to_lowercase(), "schema": b.schema,
        }),
    )?;
    let mut pending_edges: Vec<(u64, u64, &str, String)> = Vec::new();
    for item in &b.items {
        let ev = node(
            &mut ids,
            format!("doubledoor:evidence:{}", item.id),
            "doubledoor_evidence",
            json!({
                "provider": item.provider,
                "kind": format!("{:?}", item.kind).to_lowercase(),
                "tier": format!("{:?}", item.tier).to_lowercase(),
                "verdict": format!("{:?}", item.verdict).to_lowercase(),
                "summary": item.summary,
            }),
        )?;
        pending_edges.push((
            cand,
            ev,
            "supported-by",
            format!("{}:{}", b.candidate, item.id),
        ));
        if let Some(s) = &item.subject {
            let sid = node(
                &mut ids,
                format!("doubledoor:surface:{s}"),
                "doubledoor_surface",
                json!({ "surface": s }),
            )?;
            pending_edges.push((ev, sid, "about", format!("{}:{s}", item.id)));
        }
    }
    for (from, to) in edges {
        let a = node(
            &mut ids,
            format!("doubledoor:surface:{from}"),
            "doubledoor_surface",
            json!({ "surface": from }),
        )?;
        let z = node(
            &mut ids,
            format!("doubledoor:surface:{to}"),
            "doubledoor_surface",
            json!({ "surface": to }),
        )?;
        // The software ontology's relationship name (padagonia.software-ontology/v1).
        pending_edges.push((a, z, "depends-on", format!("{from}->{to}")));
    }
    let mut count = ids.len();
    for (src, dst, label, key) in pending_edges {
        c.post(
            "/api/v1/edges",
            &json!({
                "namespace": c.namespace,
                "external_id": format!("doubledoor:edge:{label}:{key}"),
                "idempotency_key": format!("doubledoor:edge:{label}:{key}"),
                "src": src, "dst": dst, "label": label,
                "provenance": provenance(vec![b.candidate.clone()]),
            }),
        )?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dechunks_http_bodies() {
        assert_eq!(
            dechunk("7\r\n{\"id\":1\r\n1\r\n}\r\n0\r\n\r\n"),
            "{\"id\":1}"
        );
    }

    #[test]
    fn disabled_and_misconfigured_sinks_never_fail_the_report() {
        let bundle = EvidenceBundle {
            schema: "x".into(),
            candidate: "c".into(),
            commit: "0".into(),
            items: vec![],
            providers: vec![],
            conflicts: vec![],
            explanation: Default::default(),
            assessment: crate::unify::assess(&[]),
        };
        assert_eq!(
            publish(&PadagoniaConfig::default(), &bundle, &[]).status,
            ProviderStatus::Disabled
        );
        let https = PadagoniaConfig {
            enabled: true,
            url: "https://x".into(),
            ..Default::default()
        };
        assert!(matches!(
            publish(&https, &bundle, &[]).status,
            ProviderStatus::Unavailable { .. }
        ));
        let nokey = PadagoniaConfig {
            enabled: true,
            api_key_env: "DD_TEST_UNSET_KEY_VAR".into(),
            ..Default::default()
        };
        assert!(matches!(
            publish(&nokey, &bundle, &[]).status,
            ProviderStatus::Unavailable { .. }
        ));
    }
}
