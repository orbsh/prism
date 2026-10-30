//! Node mounts (gravity ADR-0001): what makes a node's language callable.
//!
//! A mount is a `Remote` booth type. The alias names the node (its
//! outbound dial-in connection is the only door, held by the realm) and the
//! `runner` is a FIXED deployment artifact — the program that receives an
//! operation's code and arguments as call arguments and executes them.
//! Nothing about a mount varies per skill or per version: the skill script
//! travels as an argument of the call, so an emergent skill (or a new version
//! of one) mints no type, no ns, no route, no definition row.
//!
//! Registration is idempotent per boot: `Remote` definitions are not
//! persisted (aura 4.5b's scope is script booths), so the manifest is the
//! durable form and boot replays it. A declaration names BOTH axes, so
//! neither has to be inferred from the other: `carrier` is the node facility
//! the call runs in — the axis `Frame::Register.carriers` announces, and the
//! only one a node can lack — while `language` is the OPERATION language the
//! mount serves, i.e. the axis a skill's declared language is matched
//! against. A mount's name carries nothing: it is just the callable type name
//! (ADR-0001 §7 — making the operation language a first-class manifest field
//! is no longer open).
//!
//! Everything here is a declared deployment fact, never guessed: a mount
//! whose entry is missing a field is a boot error, not a default.

use aura_booth::{Body, BoothType, Encoding};
use aura_engine::Engine;
use serde_json::Value;
use std::collections::BTreeMap;

/// One mount declaration: (node alias × operation language) → the runner
/// serving it.
#[derive(Clone, Debug)]
pub struct MountDecl {
    /// The callable type name — what `ctx.invoke` addresses.
    pub type_name: String,
    /// The node's alias; must equal the dial-in path/frame alias.
    pub node_alias: String,
    /// The carrier the call runs in (`exec`, `induction`, …) — the node must
    /// carry it or the call is an error value.
    pub carrier: String,
    /// The OPERATION language this mount serves (`nu`, `python`, …): the axis
    /// a skill's declared language is matched against when routing. A
    /// declaration, not a dispatch mechanism — the runner spec is what runs,
    /// and per ADR-0001 §7 the LLM never picks a carrier.
    pub language: String,
    /// The runner: the process carrier's argv spec, or a carrier's source.
    pub runner: String,
    /// The frame codec for the process carriers (JSON default — a runner
    /// script cannot speak CBOR; an induction guest may declare it).
    pub encoding: Encoding,
}

impl MountDecl {
    /// Parse one manifest entry: `{"type", "alias", "carrier", "language",
    /// "runner", "encoding"?}`.
    pub fn from_json(v: &Value) -> anyhow::Result<Self> {
        let s = |k: &str| -> anyhow::Result<String> {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("mount manifest entry missing string `{k}`: {v}"))
        };
        let encoding = match v.get("encoding").and_then(Value::as_str) {
            None | Some("json") => Encoding::Json,
            Some("cbor") => Encoding::Cbor,
            Some(other) => anyhow::bail!("mount manifest: unknown encoding `{other}`"),
        };
        Ok(Self {
            type_name: s("type")?,
            node_alias: s("alias")?,
            carrier: s("carrier")?,
            language: s("language")?,
            runner: s("runner")?,
            encoding,
        })
    }

    /// The booth type this declaration registers.
    pub fn booth_type(&self) -> BoothType {
        BoothType {
            name: self.type_name.clone(),
            body: Body::Remote {
                node_alias: self.node_alias.clone(),
                // The body's `language` IS the carrier here: it is what the
                // node's dispatch selects a session runtime with (aura's
                // spawn_session key), not the operation's language.
                language: self.carrier.clone(),
                source: self.runner.clone(),
                encoding: self.encoding,
            },
            // No per-type TTL: a mount holds no local session state, and the
            // node's own residency is the node's business.
            idle_ttl: None,
            max_exec: None,
            hooks: BTreeMap::new(),
            receives: vec![],
        }
    }
}

/// Read the deployment manifest from `PRISM_MOUNTS` — a JSON array of mount
/// entries. Absent or empty is legal: an install with no nodes serves echo,
/// identity, and turns-over-memory only.
pub fn from_env() -> anyhow::Result<Vec<MountDecl>> {
    let Some(raw) = std::env::var("PRISM_MOUNTS")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        return Ok(Vec::new());
    };
    let parsed: Value =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("PRISM_MOUNTS is not JSON: {e}"))?;
    let entries = parsed
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("PRISM_MOUNTS must be a JSON array of mount entries"))?;
    entries.iter().map(MountDecl::from_json).collect()
}

/// The CARRIERS the manifest expects for a node alias — the other half of the
/// carrier↔node fact (the dial-in frame announces what the node was BUILT
/// with; the manifest says what its mounts' calls run in). The operation
/// language is deliberately NOT part of this comparison: a node cannot lack a
/// language it was never asked to carry — an operation the node's mounts
/// cannot serve is named per call by the runner, and the manifest's
/// `language` field is the axis a skill is matched on, not a node capability.
pub fn declared_carriers(mounts: &[MountDecl], alias: &str) -> Vec<String> {
    mounts
        .iter()
        .filter(|m| m.node_alias == alias)
        .map(|m| m.carrier.clone())
        .collect()
}

/// One line stating a carrier mismatch between the manifest and what a node
/// announced, or `None` when they agree (or when nothing is declared for this
/// alias). Pure — the manifest is an argument.
pub fn drift_line(mounts: &[MountDecl], alias: &str, announced: &[String]) -> Option<String> {
    let want = declared_carriers(mounts, alias);
    if want.is_empty() {
        return None;
    }
    let missing: Vec<&str> = want
        .iter()
        .map(String::as_str)
        .filter(|l| !announced.iter().any(|a| a == l))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "prism: node '{alias}' announced carriers {announced:?} but its mounts declare {want:?} \
         — not carried: {missing:?}"
    ))
}

/// The dial-in path's form of [`drift_line`]: reads the manifest from the
/// environment (a boot fact; cheap to re-read) and reports an unreadable
/// manifest here rather than swallowing it — the connection path has nowhere
/// to return it.
pub fn dial_in_note(alias: &str, announced: &[String]) -> Option<String> {
    match from_env() {
        Ok(mounts) => drift_line(&mounts, alias, announced),
        Err(e) => Some(format!(
            "prism: mount manifest unreadable at '{alias}' registration: {e}"
        )),
    }
}

/// Register every declared mount, returning the type names that went live.
/// One bad entry is reported and skipped rather than taking the gateway down
/// (the same tolerance `with_echoes` shows for a carrier a build lacks).
pub async fn register_all(engine: &Engine, mounts: &[MountDecl]) -> anyhow::Result<Vec<String>> {
    let mut live = Vec::new();
    for m in mounts {
        match engine.register(m.booth_type()).await {
            Ok(()) => live.push(m.type_name.clone()),
            Err(err) => eprintln!(
                "prism: mount [{}] not registered (alias {}): {err}",
                m.type_name, m.node_alias
            ),
        }
    }
    Ok(live)
}
