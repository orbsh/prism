//! The admin HTTP surface (ADR-0015 §5's curl-first endpoints, ADR-0017
//! §5's `/admin` prefix): the node approval face, riding the SAME
//! accept loop as `/ws` and `/code/...` (the raw-head branch of
//! `code_export`). JSON in, JSON out, one record behind it.
//!
//! ```text
//! POST   /admin/nodes                    {alias}              → 201 {token, status, created_at}
//!                                        (the token is shown ONCE; the registry keeps its digest)
//! GET    /admin/nodes                                            → [{alias, credential, status, created_at}]
//! GET    /admin/dead                                             → [{event, reason, data}]
//!   The engine's dead ring: events that reached nobody, and deliveries that reached a queue but
//!   could not call the body (`delivery-failed`, which carries the entry name and the error).
//! POST   /admin/nodes/{alias}/approve    {credential?}      → 200 approved record
//! DELETE /admin/nodes/{alias}            {credential?}      → 200 {revoked: n}
//! ```
//!
//! Authorization is deliberately absent in this landing: the interim
//! rule (§5, "curl-first until accounts exist") plus the deployment
//! boundary the posture names (ADR-0015 §7 — `open` deployments live
//! behind a network they trust). ADR-0017 §7's account folding (step ③,
//! records hanging under the logged-in user) lands with the account
//! surface, not here — these endpoints are exactly where they will hang.
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
        // Mint: the operator names the alias, the plane invents the token and keeps only its
        // digest. This response is the ONE place the token exists in readable form (ADR-0003) —
        // hand it to the node (config `token`, or `--token` for the container form) and forget it.
        ("POST", "/nodes") => match parse_alias(body) {
            Ok(alias) => {
                let (token, credential) = crate::nodes::mint_token();
                match nodes.create(&alias, &credential) {
                    Ok(row) => {
                        let mut out = row_json(&row);
                        out["token"] = json!(token);
                        json_response(201, &out)
                    }
                    Err(e) => plain(400, &e.to_string()),
                }
            }
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


/// `POST /admin/resources` — declare a resource, and optionally the key it uses:
///
/// ```text
/// { "owner": "<uuid>", "kind": "git", "seq": 1, "desc": "...", "source": "https://…",
///   "at": "main", "mode": "rw",
///   "secret": { "name": "gitea-token", "value": "…" }        // optional
/// }
/// ```
///
/// `owner` is taken from the body for the same reason the node face takes an alias: there are no
/// accounts behind this surface yet (ADR-0017 §7 step ③ lands them). The secret's VALUE never
/// enters the row — the row names the entry, and `GET` below cannot read it back.
pub(crate) async fn serve_resources(
    gw: &crate::Gateway,
    method: &str,
    path: &str,
    body: &[u8],
) -> Option<Vec<u8>> {
    use aura_realm::resources::Id;
    let rest = path.strip_prefix("/admin/resources")?;
    // The query belongs to the URL, not to the path: splitting on it here is what keeps `7?owner=x`
    // from being read as a sequence number.
    let rest = rest.split_once('?').map_or(rest, |(head, _)| head);
    let registry = gw.engine.realm.lock().await.resources.clone();
    let response = match (method, rest) {
        ("POST", "") => match parse_resource(body) {
            Ok((id, mut row, mode)) => {
                if !row.secret_name.is_empty() {
                    match parse_secret(body) {
                        Ok(Some((name, value))) => {
                            // A new secret entry per declaration only when the caller supplies one;
                            // re-declaring keeps the number it already had.
                            let seq = row.secret.max(1);
                            if let Err(e) = registry.secret(&id.owner, seq, &name, &value) {
                                return Some(plain(500, &e.to_string()));
                            }
                            row.secret = seq;
                        }
                        Ok(None) => {}
                        Err(e) => return Some(plain(400, &e.to_string())),
                    }
                }
                match registry.declare(&id, &row, mode, crate::identity::now_ms()) {
                    Ok(()) => match registry.row(&id) {
                        Ok(Some(stored)) => json_response(201, &stored),
                        Ok(None) => plain(500, "the row vanished after the write"),
                        Err(e) => plain(500, &e.to_string()),
                    },
                    Err(e) => plain(400, &e.to_string()),
                }
            }
            Err(e) => plain(400, &e.to_string()),
        },
        ("GET", other) => {
            let parts: Vec<&str> = other.trim_start_matches('/').split('/').collect();
            match parts.as_slice() {
                // The owner rides the PATH, not a query: this accept loop's head parser hands
                // handlers a path without the query, and a route that depended on one would be a
                // 400 nobody can fix by reading the URL.
                [owner, kind, seq] => match seq.parse::<u64>() {
                    Ok(seq) => match Id::new(owner, kind, seq) {
                        Ok(id) => match registry.row(&id) {
                            Ok(Some(stored)) => {
                                let mut out = stored;
                                out["version"] = json!(registry.current(&id).unwrap_or(0));
                                json_response(200, &out)
                            }
                            Ok(None) => plain(404, "no such resource"),
                            Err(e) => plain(500, &e.to_string()),
                        },
                        Err(e) => plain(400, &e.to_string()),
                    },
                    Err(_) => plain(400, "the sequence must be a number"),
                },
                _ => plain(404, "unknown admin route"),
            }
        }
        _ => plain(404, "unknown admin route"),
    };
    Some(response)
}

fn parse_resource(
    body: &[u8],
) -> anyhow::Result<(aura_realm::resources::Id, aura_realm::resources::Row, aura_realm::resources::Mode)> {
    use aura_realm::resources::{Id, Mode, Row};
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("body must be JSON: {e}"))?;
    let text = |name: &str| -> anyhow::Result<String> {
        v.get(name)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("`{name}` must be a string"))
    };
    let id = Id::new(
        &text("owner")?,
        &text("kind")?,
        v.get("seq")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| anyhow::anyhow!("`seq` must be a number"))?,
    )?;
    let mode = match v.get("mode").and_then(|x| x.as_str()) {
        Some(text) => Mode::parse(text)?,
        None => Mode::ReadOnly,
    };
    Ok((
        id,
        Row {
            desc: v.get("desc").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            source: text("source")?,
            at: v.get("at").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            secret: 0,
            secret_name: v
                .get("secret")
                .and_then(|s| s.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
        },
        mode,
    ))
}

fn parse_secret(body: &[u8]) -> anyhow::Result<Option<(String, String)>> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("body must be JSON: {e}"))?;
    let Some(secret) = v.get("secret") else {
        return Ok(None);
    };
    let name = secret
        .get("name")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("`secret.name` must be a non-empty string"))?;
    let value = secret
        .get("value")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("`secret.value` must be a non-empty string"))?;
    Ok(Some((name.to_string(), value.to_string())))
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
    // The device id is a uuid, so it arrives as its text spelling — the only form a JSON body can
    // carry without losing the low bits.
    let Some(device_id) = v
        .get("device_id")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<crate::identity::DeviceId>().ok())
    else {
        return Some(plain(400, "`device_id` (uuid text) is required"));
    };
    match gw.revoke_device(device_id) {
        Ok(n) => Some(json_response(200, &json!({"device_id": device_id.to_string(), "kicked": n}))),
        Err(e) => Some(plain(409, &e.to_string())),
    }
}

fn row_json(n: &crate::nodes::Node) -> serde_json::Value {
    json!({
        "alias": n.alias,
        "credential": n.credential,
        "status": n.status,
        "created_at": n.created_at,
    })
}

fn parse_alias(body: &[u8]) -> anyhow::Result<String> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("body must be JSON: {e}"))?;
    let field = |k: &str| -> anyhow::Result<String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("`{k}` is required"))
    };
    field("alias")
}

/// The optional `credential` on approve/revoke bodies: absent (or an
/// empty body) = None; present but not a non-empty string = error.
fn optional_key(body: &[u8]) -> anyhow::Result<Option<String>> {
    if body.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("body must be JSON: {e}"))?;
    match v.get("credential") {
        None => Ok(None),
        Some(x) => x
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| anyhow::anyhow!("`credential` must be a non-empty string")),
    }
}


#[cfg(test)]
mod resource_surface_tests {
    use super::{parse_resource, parse_secret};

    #[test]
    fn a_declaration_needs_an_owner_a_known_kind_and_a_source() {
        let ok = br#"{"owner":"alice","kind":"git","seq":7,"source":"https://g/x.git","mode":"rw"}"#;
        let (id, row, mode) = parse_resource(ok).expect("a complete declaration parses");
        assert_eq!(id.owner, "alice");
        assert_eq!(id.kind, "git");
        assert_eq!(id.seq, 7);
        assert_eq!(row.source, "https://g/x.git");
        assert_eq!(mode.as_str(), "rw");

        // Omitting the mode is read-only: a missing word never grants a write.
        let bare = br#"{"owner":"alice","kind":"s3","seq":1,"source":"s3://a/in"}"#;
        assert_eq!(parse_resource(bare).unwrap().2.as_str(), "ro");

        // A kind the venue cannot serve is refused BY NAME here, before any call runs.
        let bad = br#"{"owner":"alice","kind":"table","seq":1,"source":"x"}"#;
        let err = parse_resource(bad).unwrap_err().to_string();
        assert!(err.contains("table"), "{err}");
    }

    #[test]
    fn a_secret_is_optional_but_never_half_given() {
        assert!(parse_secret(b"{}").unwrap().is_none());
        assert!(parse_secret(br#"{"secret":{"name":"t","value":"v"}}"#).is_ok());
        assert!(parse_secret(br#"{"secret":{"name":"t"}}"#).is_err());
        assert!(parse_secret(br#"{"secret":{"name":"","value":"v"}}"#).is_err());
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
