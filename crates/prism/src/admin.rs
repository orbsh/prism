//! The admin HTTP surface (ADR-0015 §5's curl-first endpoints, ADR-0017
//! §5's `/admin` prefix): the node approval face, riding the SAME
//! accept loop as `/ws` and `/code/...` (the raw-head branch of
//! `code_export`). JSON in, JSON out, one record behind it.
//!
//! ```text
//! POST   /admin/nodes                    {alias, public_key}  → 201 pending record
//! GET    /admin/nodes                                            → [{alias, public_key, status, created_at}]
//! POST   /admin/nodes/{alias}/approve    {public_key?}        → 200 approved record
//! DELETE /admin/nodes/{alias}            {public_key?}        → 200 {revoked: n}
//! ```
//!
//! Authorization is deliberately absent in this landing: the interim
//! rule (§5, "curl-first until accounts exist") plus the deployment
//! boundary the posture names (ADR-0015 §7 — `open` deployments live
//! behind a network they trust). ADR-0017 §5's account folding (step ③,
//! records hanging under the logged-in user) lands with the account
//! session surface, not here — these endpoints are exactly where they
//! will hang.
//!
//! Failure shapes: a malformed or missing field = 400; a decision the
//! registry refuses (ambiguous pending queue, no such record) = 409 —
//! the operator's request is well-formed but the registry says no; the
//! response carries the registry's own message. Status is the machine
//! answer; the body text is for the human reading `curl` output.

use crate::code_export::plain;
use crate::nodes::NodeStore;
use serde_json::json;

/// Route an `/admin/...` request. Returns the full response bytes for
/// any path under the prefix (the connection closes after), and None
/// for anything else (the caller falls through to `/ws` or the export).
pub(crate) fn serve_admin(nodes: &NodeStore, method: &str, path: &str, body: &[u8]) -> Option<Vec<u8>> {
    let rest = path.strip_prefix("/admin")?;
    let response = match (method, rest) {
        ("POST", "/nodes") => match parse_pair(body) {
            Ok((alias, public_key)) => match nodes.create(&alias, &public_key) {
                Ok(row) => json_response(201, &row_json(&row)),
                Err(e) => plain(400, &e.to_string()),
            },
            Err(e) => plain(400, &e.to_string()),
        },
        ("GET", "/nodes") => {
            let rows: Vec<_> = nodes.list().iter().map(row_json).collect();
            json_response(200, &json!(rows))
        }
        ("POST", other) => match other.strip_prefix("/nodes/").and_then(|a| a.strip_suffix("/approve")) {
            Some(alias) => {
                // The key is optional (unique-pending approval); empty
                // body or absent field = None, present-but-blank = 400.
                let key = optional_key(body);
                match key {
                    Ok(key) => match nodes.approve(alias, key.as_deref()) {
                        Ok(row) => json_response(200, &row_json(&row)),
                        Err(e) => plain(409, &e.to_string()),
                    },
                    Err(e) => plain(400, &e.to_string()),
                }
            }
            None => plain(404, "unknown admin route"),
        },
        ("DELETE", other) => match other.strip_prefix("/nodes/") {
            Some(alias) if !alias.is_empty() && !alias.contains('/') => {
                let key = optional_key(body);
                match key {
                    Ok(key) => match nodes.revoke(alias, key.as_deref()) {
                        Ok(n) => json_response(200, &json!({"revoked": n})),
                        Err(e) => plain(409, &e.to_string()),
                    },
                    Err(e) => plain(400, &e.to_string()),
                }
            }
            _ => plain(404, "unknown admin route"),
        },
        _ => plain(404, "unknown admin route"),
    };
    Some(response)
}

fn row_json(n: &crate::nodes::Node) -> serde_json::Value {
    json!({
        "alias": n.alias,
        "public_key": n.public_key,
        "status": n.status,
        "created_at": n.created_at,
    })
}

fn parse_pair(body: &[u8]) -> anyhow::Result<(String, String)> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("body must be JSON: {e}"))?;
    let field = |k: &str| -> anyhow::Result<String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("`{k}` is required"))
    };
    Ok((field("alias")?, field("public_key")?))
}

/// The optional `public_key` on approve/revoke bodies: absent (or an
/// empty body) = None; present but not a non-empty string = error.
fn optional_key(body: &[u8]) -> anyhow::Result<Option<String>> {
    if body.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("body must be JSON: {e}"))?;
    match v.get("public_key") {
        None => Ok(None),
        Some(x) => x
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| anyhow::anyhow!("`public_key` must be a non-empty string")),
    }
}

pub(crate) fn json_response(status: u16, value: &serde_json::Value) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "OK",
    };
    let body = serde_json::to_vec(value).expect("json! output always serializes");
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(&body);
    out
}
