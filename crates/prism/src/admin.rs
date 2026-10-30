//! The admin HTTP surface (ADR-0015 §5's curl-first endpoints, ADR-0017
//! §5's `/admin` prefix): the node approval face, riding the SAME
//! accept loop as `/ws` and `/code/...` (the raw-head branch of
//! `code_export`). JSON in, JSON out, one record behind it.
//!
//! ```text
//! POST   /admin/nodes                    {alias, public_key}  → 201 pending record
//! GET    /admin/nodes                                            → [{alias, public_key, status, created_at}]
//! GET    /admin/dead                                             → [{event, reason, data}]
//!   The engine's dead ring: events that reached nobody, and deliveries that reached a queue but
//!   could not call the body (`delivery-failed`, which carries the entry name and the error).
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

/// `GET /admin/dead` — the dead ring behind an HTTP read. Async because the ring lives behind the
/// realm lock; every other `/admin` route is a plain store read. Ask this BEFORE `serve_admin`:
/// that one claims the whole prefix and answers 404 for what it does not know.
pub(crate) async fn serve_dead(
    gw: &crate::Gateway,
    method: &str,
    path: &str,
) -> Option<Vec<u8>> {
    if (method, path) != ("GET", "/admin/dead") {
        return None;
    }
    let rows = {
        let realm = gw.engine.realm.lock().await;
        realm
            .dead_events
            .detailed()
            .iter()
            .map(|(event, data, reason)| {
                json!({ "event": event, "reason": reason.to_string(), "data": data })
            })
            .collect::<Vec<_>>()
    };
    Some(json_response(200, &json!(rows)))
}

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
        ("GET", "/nodes") => match nodes.list() {
            Ok(rows) => {
                let rows: Vec<_> = rows.iter().map(row_json).collect();
                json_response(200, &json!(rows))
            }
            // The engine could not answer the read: that is the server's
            // own fault, not the operator's request — 500, with its text.
            Err(e) => plain(500, &e.to_string()),
        },
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

/// Route `POST /admin/revoke {device_id}` — the device revocation face
/// (Phase 1 tail). Needs the whole gateway: the registry call moves the
/// durable facts, the connection-table pass kicks live sockets. Same
/// failure shapes as the node face: malformed body = 400, registry
/// refusal (unknown device) = 409 with the registry's message.
pub(crate) async fn serve_revoke(
    gw: &crate::Gateway,
    method: &str,
    path: &str,
    body: &[u8],
) -> Option<Vec<u8>> {
    if method != "POST" || path != "/admin/revoke" {
        // Not ours (or wrong verb) — fall through; /admin itself is
        // serve_admin's prefix.
        return if path.starts_with("/admin/revoke") {
            Some(plain(404, "unknown admin route"))
        } else {
            None
        };
    }
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return Some(plain(400, &format!("body must be JSON: {e}"))),
    };
    let Some(device_id) = v.get("device_id").and_then(|x| x.as_u64()) else {
        return Some(plain(400, "`device_id` (u64) is required"));
    };
    match gw.revoke_device(device_id) {
        Ok(n) => Some(json_response(200, &json!({"device_id": device_id, "kicked": n}))),
        Err(e) => Some(plain(409, &e.to_string())),
    }
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
        500 => "Internal Server Error",
        _ => "OK",
    };
    // `value` is a `serde_json::Value` built by `json!`: every shape serde can
    // reject (non-string map keys, NaN floats) is unrepresentable in it, so the
    // encode is infallible by construction.
    #[allow(clippy::expect_used)]
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
