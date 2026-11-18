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


/// `POST /admin/resources` — declare what this venue offers for one KIND, and optionally the
/// credential that reaches it (ADR-0049: a resource is a grant, not a snapshot).
///
/// ```text
/// { "kind": "object", "desc": "...", "base": "s3://test", "mode": "rw",
///   "secret": { "name": "rustfs", "value": "access_key:secret_key:region" }   // optional
/// }
/// ```
///
/// `GET /admin/resources` lists every declared grant; `GET /admin/resources/<kind>` shows one;
/// `DELETE /admin/resources/<kind>` withdraws it — the grant row AND its credential, because a key
/// nobody can reach through is worse than no key (a later declaration would silently inherit it).
/// There is no per-resource route and no version route: what a script names inside the grant is the
/// script's business, and the store keeps its own history.
pub(crate) async fn serve_resources(
    gw: &crate::Gateway,
    method: &str,
    path: &str,
    body: &[u8],
) -> Option<Vec<u8>> {
    let rest = path.strip_prefix("/admin/resources")?;
    // The query belongs to the URL, not to the path.
    let rest = rest.split_once('?').map_or(rest, |(head, _)| head);
    let registry = gw.engine.realm.lock().await.resources.clone();
    let response = match (method, rest) {
        ("POST", "") => match parse_grant(body) {
            Ok((kind, mut grant)) => {
                match parse_secret(body) {
                    Ok(Some((name, value))) => {
                        // The credential is visible here and nowhere else — the venue stores it and
                        // never reads it back, so this door is the only place its shape can be
                        // checked. The rule is `effector_protocol::resource`'s, the same code the
                        // node runs before it signs a request.
                        if let Err(e) = declaration_shape(&kind, Some(&value)) {
                            return Some(plain(400, &e));
                        }
                        if let Err(e) = registry.secret(&kind, &value) {
                            return Some(plain(500, &e.to_string()));
                        }
                        grant.secret_name = name;
                    }
                    // A declaration with no credential still has to be servable: the KIND is checked
                    // either way, or `archive` would slip in through the empty-secret door.
                    Ok(None) => {
                        if let Err(e) = declaration_shape(&kind, None) {
                            return Some(plain(400, &e));
                        }
                    }
                    Err(e) => return Some(plain(400, &e.to_string())),
                }
                match registry.declare(&kind, &grant, crate::identity::now_ms()) {
                    Ok(()) => match registry.grant(&kind) {
                        Ok(Some(stored)) => json_response(201, &stored),
                        Ok(None) => plain(500, "the grant vanished after the write"),
                        Err(e) => plain(500, &e.to_string()),
                    },
                    Err(e) => plain(400, &e.to_string()),
                }
            }
            Err(e) => plain(400, &e.to_string()),
        },
        // The LISTING (`GET /admin/resources`). A management surface that can only be read one kind
        // at a time is a surface you cannot operate from: "what is declared here" is the first
        // question an operator asks, and answering it by guessing the kinds is not an answer.
        ("GET", "") => match registry.declared() {
            Ok(rows) => json_response(
                200,
                &serde_json::Value::Array(
                    rows.into_iter()
                        .map(|(kind, row)| serde_json::json!({ "kind": kind, "grant": row }))
                        .collect(),
                ),
            ),
            Err(e) => plain(500, &e.to_string()),
        },
        ("GET", other) => {
            let kind = other.trim_start_matches('/');
            if kind.is_empty() || kind.contains('/') {
                return Some(plain(404, "unknown admin route"));
            }
            match registry.grant(kind) {
                Ok(Some(stored)) => json_response(200, &stored),
                Ok(None) => plain(404, "no such grant"),
                Err(e) => plain(500, &e.to_string()),
            }
        }
        // WITHDRAW (`DELETE /admin/resources/<kind>`). Both rows go: the grant and its credential.
        // Keeping the key would leave something nobody can reach through — and something a later
        // re-declare would silently inherit, which is the opposite of "withdrawn".
        ("DELETE", other) => {
            let kind = other.trim_start_matches('/');
            if kind.is_empty() || kind.contains('/') {
                return Some(plain(404, "unknown admin route"));
            }
            match registry.grant(kind) {
                Ok(None) => plain(404, "no such grant"),
                Ok(Some(_)) => match registry.revoke(kind) {
                    Ok(()) => json_response(200, &serde_json::json!({ "revoked": kind })),
                    Err(e) => plain(500, &e.to_string()),
                },
                Err(e) => plain(500, &e.to_string()),
            }
        }
        _ => plain(404, "unknown admin route"),
    };
    Some(response)
}

/// What a declaration must satisfy before it is stored — asked of every kind, with or without a
/// credential attached.
///
/// First: is this a KIND anything can move? The protocol's list answers, and it is the same list the
/// node checks before it signs a request. `archive` is refused by the node BY NAME and is not
/// declared here either: a grant for it would fail at the first task, which is the lie a management
/// surface must not tell.
///
/// Second: does the credential have the shape its kind needs? `git` takes whatever the host issues
/// (a token); `object` takes `access_key:secret_key[:region]`.
pub(crate) fn declaration_shape(kind: &str, value: Option<&str>) -> Result<(), String> {
    if !effector_protocol::resource::is_kind(kind) {
        return Err(format!(
            "`{kind}` is not a resource kind this venue serves ({}); a node refuses it by name, so a \
             grant for it would only fail at the first task",
            effector_protocol::resource::KINDS.join(", ")
        ));
    }
    match (kind, value) {
        ("object", Some(v)) => effector_protocol::resource::credential_parts(v)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        _ => Ok(()),
    }
}

fn parse_grant(body: &[u8]) -> anyhow::Result<(String, aura_realm::resources::Grant)> {
    use aura_realm::resources::{Grant, Mode};
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("body must be JSON: {e}"))?;
    let text = |name: &str| -> anyhow::Result<String> {
        v.get(name)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("`{name}` must be a string"))
    };
    let kind = text("kind")?;
    let mode = match v.get("mode").and_then(|x| x.as_str()) {
        Some(text) => Mode::parse(text)?,
        None => Mode::ReadOnly,
    };
    Ok((
        kind,
        Grant {
            desc: v
                .get("desc")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            base: text("base")?,
            mode,
            secret_name: String::new(),
        },
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

#[cfg(test)]
mod resource_surface_tests {
    use super::{declaration_shape, parse_grant, parse_secret};

    #[test]
    fn a_grant_needs_a_kind_and_a_base() {
        let ok = br#"{"kind":"git","base":"https://gitea.s","mode":"rw"}"#;
        let (kind, grant) = parse_grant(ok).expect("a complete grant parses");
        assert_eq!(kind, "git");
        assert_eq!(grant.base, "https://gitea.s");
        assert_eq!(grant.mode.as_str(), "rw");

        // Omitting the mode is read-only: a missing word never grants a write.
        let bare = br#"{"kind":"object","base":"s3://bucket"}"#;
        assert_eq!(parse_grant(bare).unwrap().1.mode.as_str(), "ro");

        // A base is what makes a grant reachable; without one there is nothing to declare.
        let bad = br#"{"kind":"git"}"#;
        let err = parse_grant(bad).unwrap_err().to_string();
        assert!(err.contains("base"), "{err}");
    }

    #[test]
    fn an_object_credential_has_the_shape_the_node_will_sign_with() {
        assert!(declaration_shape("object", Some("ak:sk")).is_ok());
        assert!(declaration_shape("object", Some("ak:sk:us-east-1")).is_ok());
        let err = declaration_shape("object", Some("not-a-pair")).unwrap_err();
        assert!(err.contains("access_key:secret_key"), "{err}");
        // A token for git has no shape this plane can judge.
        assert!(declaration_shape("git", Some("any-token")).is_ok());
        // …but the KIND is judged for every declaration, credential or not: `archive` is a word
        // the node refuses by name, and a grant for it would only fail at the first task.
        let err = declaration_shape("archive", None).unwrap_err();
        assert!(err.contains("archive"), "{err}");
        assert!(declaration_shape("git", None).is_ok());
    }

    #[test]
    fn a_secret_is_optional_but_never_half_given() {
        assert!(parse_secret(b"{}").unwrap().is_none());
        assert!(parse_secret(br#"{"secret":{"name":"t","value":"v"}}"#).is_ok());
        assert!(parse_secret(br#"{"secret":{"name":"t"}}"#).is_err());
        assert!(parse_secret(br#"{"secret":{"name":"","value":"v"}}"#).is_err());
    }

}
