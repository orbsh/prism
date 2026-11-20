//! Node mounts (gravity ADR-0001): what makes a node's language callable.
//!
//! THE DECLARATION SPLIT (prism ADR-0022). A mount used to be one line of a per-alias manifest, so
//! a machine that dialled in without a line was in the pool and uncallable. Two facts had been
//! travelling together and belong to different owners:
//!
//! - THE VOCABULARY — which operation languages this installation can run, each with its carrier,
//!   runner artifact, codec and required isolation. A deployment fact, declared once, the same for
//!   every node (`PRISM_LANGUAGES`).
//! - THE ALIAS LIST — which machines are nodes. That list is the node registry's, already: a row
//!   is approved or it is not.
//!
//! A mount is then DERIVED: one per (approved alias × vocabulary entry). Nothing a skill, an
//! operation or a dialling robot says can add an entry to the vocabulary — the ruling in ADR-0001
//! ("no data path mints mounts") is kept, and what a dial can do in `open` posture is stated at the
//! dial (§`install_mounts`' counterpart in the mount handler).
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

/// One entry of the deployment's language vocabulary: what this installation runs, and with which
/// artifact. Per node it becomes one mount; the node chooses nothing about it.
#[derive(Clone, Debug)]
pub struct Language {
    /// The OPERATION language a skill declares (`nu`, `python`) — the axis gravity routes on.
    pub language: String,
    /// The node facility the call runs in (`exec`, `induction`, …) — what a node can lack, which is
    /// why the dial compares the announced carriers against this.
    pub carrier: String,
    /// The fixed runner deployment artifact.
    pub runner: String,
    pub encoding: Encoding,
    pub requires: Option<aura_booth::Requires>,
}

impl Language {
    pub fn from_json(v: &Value) -> anyhow::Result<Self> {
        let s = |k: &str| -> anyhow::Result<String> {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("language entry missing string `{k}`: {v}"))
        };
        let encoding = match v.get("encoding").and_then(Value::as_str) {
            None | Some("json") => Encoding::Json,
            Some("cbor") => Encoding::Cbor,
            Some(other) => anyhow::bail!("language entry: unknown encoding `{other}`"),
        };
        let requires = match v.get("requires").and_then(Value::as_str) {
            None => None,
            Some("container") => Some(aura_booth::Requires::Container),
            Some("host") => Some(aura_booth::Requires::Host),
            Some(other) => anyhow::bail!("language entry: unknown requires `{other}` (host|container)"),
        };
        Ok(Self {
            language: s("language")?,
            carrier: s("carrier")?,
            runner: s("runner")?,
            encoding,
            requires,
        })
    }

    /// The mount this vocabulary entry makes for one node. The type name is `{alias}.{language}`:
    /// a name, not a path — nothing parses it back apart (an alias containing a dot is refused at
    /// minting, so the pair stays unambiguous for a reader and for this construction).
    pub fn mount_for(&self, alias: &str) -> MountDecl {
        MountDecl {
            type_name: format!("{alias}.{}", self.language),
            node_alias: alias.to_string(),
            carrier: self.carrier.clone(),
            language: self.language.clone(),
            requires: self.requires,
            runner: self.runner.clone(),
            encoding: self.encoding,
        }
    }
}

/// The deployment's vocabulary, from `PRISM_LANGUAGES` — a JSON array of
/// `{language, carrier, runner, encoding?, requires?}`. Absent means this installation runs no
/// node code, which is legal and said at boot. A MALFORMED one is a boot error: it is a deployment
/// fact, never a default (ADR-0001).
pub fn vocabulary_from_env() -> anyhow::Result<Vec<Language>> {
    let Some(raw) = std::env::var("PRISM_LANGUAGES")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        return Ok(Vec::new());
    };
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("PRISM_LANGUAGES is not JSON: {e}"))?;
    let entries = parsed
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("PRISM_LANGUAGES must be a JSON array of entries"))?;
    let mut out: Vec<Language> = Vec::with_capacity(entries.len());
    for entry in entries {
        let lang = Language::from_json(entry)?;
        if out.iter().any(|l| l.language == lang.language) {
            anyhow::bail!(
                "PRISM_LANGUAGES declares `{}` twice — one language, one runner",
                lang.language
            );
        }
        out.push(lang);
    }
    Ok(out)
}

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
    /// What the node must be for this mount to run (effector ADR-0003 §1). `None` = the container:
    /// a mount that says nothing gets the separated path, and a node that cannot separate refuses
    /// the call by name. `Some(Host)` is the exception, written down by the mount.
    pub requires: Option<aura_booth::Requires>,
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
        // `requires` is absent from a manifest entry unless the mount must run on bare metal: a
        // manifest that said nothing would otherwise be the one place the container default could
        // be lost, so the field is read here and `None` keeps the default.
        let requires = match v.get("requires").and_then(Value::as_str) {
            None => None,
            Some("container") => Some(aura_booth::Requires::Container),
            Some("host") => Some(aura_booth::Requires::Host),
            Some(other) => anyhow::bail!("mount manifest: unknown requires `{other}` (host|container)"),
        };
        Ok(Self {
            type_name: s("type")?,
            node_alias: s("alias")?,
            carrier: s("carrier")?,
            language: s("language")?,
            runner: s("runner")?,
            encoding,
            requires,
        })
    }

    /// The booth type this declaration registers.
    pub fn booth_type(&self) -> BoothType {
        BoothType {
            name: self.type_name.clone(),
            body: Body::Remote {
                node_alias: self.node_alias.clone(),
                // The body's `language` IS the carrier here: it is what the
                // node's dispatch selects an executor with (aura's
                // executor key), not the operation's language.
                language: self.carrier.clone(),
                source: self.runner.clone(),
                // What this mount declares it needs (effector ADR-0003 §1). `None` = the
                // container, which is the default for everything that does not say otherwise.
                requires: self.requires,
                encoding: self.encoding,
            },
            // No per-type TTL: a mount holds no local executor state, and the
            // node's own residency is the node's business.
            idle_ttl: None,
            max_exec: None,
            hooks: BTreeMap::new(),
            receives: vec![],
            concurrency: aura_booth::Concurrency::Queued,
        }
    }
}

/// WHERE A NODE FETCHES THIS VENUE'S CODE (ADR-0027, gravity ADR-0001 §4). A mount's source is a
/// deployment artifact and the dispatch carries only its ADDRESS (content-addressed), so a venue
/// that declares mounts and cannot serve them has declared a mount that fails on the first call —
/// `no code_base_url configured` three layers below whoever asked. The venue serves the specs it
/// registers at `/code/<hex>` on its own listener, so it knows its own base.
///
/// A base derived from a BIND-ALL address is not one a node can fetch: `0.0.0.0` is a listener, not
/// a destination, so that case names `PRISM_CODE_BASE` instead of guessing a hostname.
pub fn code_base(addr: &str) -> anyhow::Result<String> {
    if let Ok(explicit) = std::env::var("PRISM_CODE_BASE") {
        let explicit = explicit.trim().trim_end_matches('/').to_string();
        if !explicit.is_empty() {
            return Ok(explicit);
        }
    }
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    if host.is_empty() || host == "0.0.0.0" || host == "[::]" || host == "::" {
        anyhow::bail!(
            "PRISM_ADDR `{addr}` binds every interface, which is not an address a node can fetch \
             from: set PRISM_CODE_BASE (e.g. `http://10.0.0.5:8765/code`) so mounts can deliver \
             their runners (ADR-0027)"
        );
    }
    Ok(format!("http://{addr}/code"))
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

/// The dial-in path reads the table the venue INSTALLED, not the environment: one source, so a
/// manifest that failed to parse said so once at boot instead of at every dial.
pub fn dial_in_note(table: &MountTable, alias: &str, announced: &[String]) -> Option<String> {
    drift_line(&table.decls, alias, announced)
}

/// WHAT THE VENUE HOLDS: the declarations it replayed, which of them registered, and why the
/// rest did not. One table, one home — a caller reads this instead of carrying its own copy of
/// the same list, which is how two clients of one venue come to disagree (ADR-0021, discharging
/// gravity ADR-0001's open item).
#[derive(Debug, Default, Clone)]
pub struct MountTable {
    /// The deployment's vocabulary: every mount in `decls` was derived from one of these entries.
    pub languages: Vec<Language>,
    /// Every declaration derived so far, whether or not it registered.
    pub decls: Vec<MountDecl>,
    /// The type names that registered.
    pub live: std::collections::BTreeSet<String>,
    /// One line per declaration that did not, in the words the failure used.
    pub refused: Vec<String>,
}

impl MountTable {
    /// An empty table for a vocabulary: mounts are added per alias as they are approved (or, in
    /// `open` posture, as they dial).
    pub fn with_vocabulary(languages: Vec<Language>) -> Self {
        Self {
            languages,
            ..Default::default()
        }
    }

    /// Derive this alias's mounts from the vocabulary, returning only the ones the table did not
    /// hold yet. Idempotent: a second dial of the same alias adds nothing.
    pub fn add_alias(&mut self, alias: &str) -> Vec<MountDecl> {
        let known: std::collections::BTreeSet<&str> =
            self.decls.iter().map(|d| d.type_name.as_str()).collect();
        let mut added = Vec::new();
        for lang in &self.languages {
            let decl = lang.mount_for(alias);
            if !known.contains(decl.type_name.as_str()) {
                added.push(decl);
            }
        }
        self.decls.extend(added.iter().cloned());
        added
    }

    /// Register declarations and record what the engine said. A failure is recorded, not fatal: a
    /// venue that refused to boot because one node's mount is wrong would take the other nodes'
    /// work down with it (the same tolerance `with_echoes` shows for a carrier a build lacks).
    pub async fn register(&mut self, engine: &Engine, decls: Vec<MountDecl>) {
        for m in decls {
            let alias = m.node_alias.clone();
            let name = m.type_name.clone();
            let outcome = engine.register(m.booth_type()).await;
            self.record(name, alias, outcome);
        }
    }

    /// Book one registration's outcome. Split out from `register` so a caller that registers
    /// outside the lock (the dial path) can still write the result in the same words.
    pub fn record(&mut self, type_name: String, alias: String, outcome: anyhow::Result<()>) {
        match outcome {
            Ok(()) => {
                self.live.insert(type_name);
            }
            Err(err) => self.refused.push(format!(
                "mount [{type_name}] (alias {alias}) did not register: {err:#}"
            )),
        }
    }

    /// The type is declared AND registered: a declared mount whose type is not in the engine
    /// would answer a caller with "no such type" long after the confusion started.
    pub fn is_live(&self, type_name: &str) -> bool {
        self.live.contains(type_name)
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
