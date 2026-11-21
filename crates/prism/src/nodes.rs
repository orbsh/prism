//! Node identity, trust plane data surface (aura ADR-0015 §4/§5 — approval by
//! digest and the curl-first endpoint set — plus this plane's own §7 for where
//! that surface hangs; prism ADR-0017 §7. The registry lives in Prism's
//! connection plane, never in aura: one home, so there is no second truth to
//! diverge from it. Labels and machine binding are prism ADR-0020.)
//!
//! What lands here (step 1 of the ADR's implementation order, prism's
//! half): the node registry — `{alias, credential, status, created_at}`
//! rows in prism's own okm instance — the verdict the handshake will
//! consult, the trust-mode posture (`identity: required | open`, no
//! default), and the approval HTTP surface (`/admin/nodes...`, wired in
//! `admin.rs`). The handshake EXECUTION (challenge frames on
//! `/effector/<alias>`) rides the effector-protocol change and the effector
//! mount — coordinated step 2; until then the verdict function is the
//! seam that stays testable.
//!
//! Registry pattern (the Account/Device/TypeName family): fixed-width
//! proxy id key + payload mirror + text index + HighWater reduce.
//! Alias is open-ended runtime DATA — it resolves through `by_alias`,
//! never as a key. A CREDENTIAL is the SHA-256 hex of the node's token
//! (ADR-0003: the authority is the token the node dialed with); the store
//! never holds the token itself and never parses the digest, only compares.
//!
//! One approved key per alias is the model that makes `conflict`
//! mean something: approving a second key REPLACES the first (§6
//! rotation — the old record falls to `revoked`, never silently
//! deleted: the history is the audit). Revocation = status transition
//! to `revoked` (the row survives; §4 lists `revoked` as a status, and
//! tombstoning an alias's live keys while keeping the fact they
//! existed is the honest version of "delete").

use anyhow::anyhow;
use okm_core::document::Collection;
use okm_core::{Document, DocumentEncode, KeyEncode};

/// The node table (prism instance, ns 52 — beside Account 50 / Device 51).
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct NodeKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(NodeKey)]
#[ok_index(by_alias { fields(alias) })]
#[ok_reduce(HighWater(id) { group(global) })]
#[ok_ns(52)]
pub struct Node {
    /// Payload mirror of the proxy id (the TypeName precedent).
    pub id: u64,
    pub alias: String,
    /// SHA-256 hex of the node's token (ADR-0003): the store keeps the digest so a
    /// dump of this table is not a set of keys. Opaque here — compared, never parsed.
    pub credential: String,
    /// `pending | approved | revoked` — stringly because it is never
    /// indexed and the vocabulary is tiny; the transitions below are
    /// the only writers.
    pub status: String,
    /// Epoch milliseconds at record creation.
    pub created_at: u64,
    /// Single-group discriminator for the registry-wide watermark.
    pub global: u32,
    /// The names the operator gave this machine in the admin UI — `MyLaptop`, `xxServer` — one
    /// per line. Bare, as typed: the user id is prepended at ADMISSION, on the way into the
    /// venue's pool, because the execution layer is where two users' `MyLaptop` have to stop
    /// colliding (aura ADR-0053). Stored bare so a rename is a rename and the history stays
    /// attached to the machine, not to a string.
    ///
    /// A string rather than a list because the payload is fixed-width TLV with no sequence
    /// type, and a name cannot contain a newline (refused at mint). Stringly for the same
    /// reason `status` is: never indexed, tiny vocabulary, one place that splits it.
    pub labels: String,
    /// The user this token was minted for (empty = the installation's own node). Labels are
    /// names, and names are per-user: it is why the prefix exists at all.
    pub owner: String,
    /// The machine this token is BOUND to (effector ADR-0006's `machine`), written on the
    /// first successful registration and never changed afterwards.
    ///
    /// One token, one machine: a token is minted for a use ("enroll my laptop"), and the box
    /// that presents it first owns it. Empty means unbound — either never used yet, or a node
    /// that could not identify its hardware, which cannot be bound and says so.
    pub machine: String,
}

impl Node {
    /// The names as the operator typed them. One place splits the row's line-joined string, so
    /// nothing downstream has to know how it was stored.
    pub fn label_list(&self) -> Vec<String> {
        self.labels
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// Mint one node token and the credential the registry stores for it.
///
/// The token is 32 random bytes, URL-safe base64 without padding — it rides a URL QUERY
/// (ADR-0003: the authority is the dial), so it must not need escaping. The credential is the
/// SHA-256 hex of it: a node token is already high-entropy randomness, so a slow KDF buys
/// nothing here; what matters is that the registry never holds the token, so a dump of the node
/// table is not a set of keys. The token is returned to the operator ONCE, at minting.
pub fn mint_token() -> (String, String) {
    use argon2::password_hash::rand_core::RngCore as _;
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    argon2::password_hash::rand_core::OsRng.fill_bytes(&mut bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let credential = token_credential(&token);
    (token, credential)
}

/// The credential a token hashes to. Public because the handshake and the tests both need to
/// ask "does this token match the row?" without ever holding a second copy of the rule.
pub fn token_credential(token: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}

use __OkmIndex_Node_by_alias as NodeByAlias;

/// The handshake verdict for a (alias, credential) claim (ADR-0015 §3's
/// table, `required` row set). Mapping decisions to names the wire
/// answers: `registered | pending | conflict`.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The key is approved for that alias, and the machine presenting it is the machine the
    /// token belongs to (or the token was unbound until now, and this registration bound it).
    ///
    /// Carries the row's labels and owner because the caller is the one that hands them to the
    /// venue's pool — prefixed there, at the execution layer, not here where they are data.
    Registered { labels: Vec<String>, owner: String },
    /// No decision yet — a fresh claim auto-registers as pending so
    /// the operator can see and approve it (`GET /nodes` then approve);
    /// an already-pending claim stays pending (idempotent retry).
    Pending,
    /// The claim is refused, and the reason travels with it: a DIFFERENT key holds the
    /// approved slot, or the same token arrives from a machine it is not bound to. Both are
    /// surfaced — an enrolled key and an enrolled machine are never silently replaced.
    Conflict { why: String },
}

/// Trust mode, a declared deployment-form switch (§7). No default: the
/// Gateway is constructed with it, the binary refuses to boot without
/// PRISM_IDENTITY. Enforcement rides the handshake step; the posture
/// declares intent and states itself at startup either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    Required,
    Open,
}

impl Posture {
    pub fn parse(v: &str) -> anyhow::Result<Self> {
        match v {
            "required" => Ok(Posture::Required),
            "open" => Ok(Posture::Open),
            other => Err(anyhow!(
                "identity: expected `required` | `open`, got `{other}`"
            )),
        }
    }
}

/// What a claim says about itself when it first becomes a row: minted by the operator
/// (`POST /admin/nodes`, with the names they typed), or presented by a node at the dial
/// (`alias`, `credential`, and whichever machine it reported).
#[derive(Default)]
pub struct Claim {
    pub alias: String,
    pub credential: String,
    pub machine: String,
    pub labels: Vec<String>,
    pub owner: String,
}

/// The node registry over prism's okm instance. Shares the store handle
/// with the account/device Registry (one fjall directory, ever).
#[derive(Clone)]
pub struct NodeStore {
    store: okm_core::FjallStore,
}

impl NodeStore {
    pub(crate) fn new(store: okm_core::FjallStore) -> Self {
        Self { store }
    }

    fn table(&self) -> anyhow::Result<Collection<okm_core::FjallStore, NodeKey, Node>> {
        // Opening attaches the declaration to the store and refuses a store of another
        // generation (ADR-0074): the only way in, and it is fallible.
        Ok(Collection::open(self.store.clone())?)
    }

    /// Handshake verdict for `required` mode. Unknown claims become
    /// pending rows (the operator's approval queue IS this table).
    /// Branch order is the §3 table: own approved row wins; then the
    /// alias being APPROVED FOR ANOTHER KEY is what makes `conflict`
    /// (an unapproved claim still queues, so the operator sees the
    /// attempt — approving the queued row is §6's rotation); a revoked
    /// row of the claimant revives to pending when nothing else holds
    /// the alias — a re-decision, never self-service.
    pub fn verdict(&self, alias: &str, credential: &str, machine: &str) -> anyhow::Result<Verdict> {
        let t = self.table()?;
        let rows = self.rows_of(alias)?;
        let mine = rows.iter().find(|n| n.credential == credential);
        if let Some(row) = mine.filter(|n| n.status == "approved") {
            // One token, one machine (ADR-0020): the first registration binds the token to the
            // hardware that presented it, and every later one must come from the same machine.
            // A node that reported no machine cannot be bound — its token then works from
            // anywhere, and that is stated rather than pretended otherwise.
            let mut row = row.clone();
            if row.machine.is_empty() {
                if machine.is_empty() {
                    eprintln!(
                        "node registry: '{alias}' registered without a machine id — this token \
                         stays unbound, so it is not confined to one piece of hardware"
                    );
                } else {
                    row.machine = machine.to_string();
                    t.put(&NodeKey { id: row.id }, &row)?;
                    eprintln!("node registry: '{alias}' token BOUND to machine `{machine}`");
                }
            } else if row.machine != machine {
                return Ok(Verdict::Conflict {
                    why: format!(
                        "this token is bound to machine `{}`, and this node reports `{}` — a \
                         token is minted for one use, and the box that presents it first owns it",
                        row.machine,
                        if machine.is_empty() {
                            "(none)"
                        } else {
                            machine
                        }
                    ),
                });
            }
            return Ok(Verdict::Registered {
                labels: row.label_list(),
                owner: row.owner.clone(),
            });
        }
        let alias_taken = rows
            .iter()
            .any(|n| n.credential != credential && n.status == "approved");
        if alias_taken {
            // Surface the attempt: queue it if it is not already a row.
            if mine.is_none() {
                self.create(&Claim {
                    alias: alias.to_string(),
                    credential: credential.to_string(),
                    machine: machine.to_string(),
                    ..Default::default()
                })?;
            }
            return Ok(Verdict::Conflict {
                why: format!(
                    "another token holds the approved slot for '{alias}' — an enrolled node is \
                     never silently replaced; revoke it first"
                ),
            });
        }
        match mine {
            Some(n) if n.status == "pending" => Ok(Verdict::Pending),
            Some(n) => {
                // revoked: revive to the queue (the operator re-decides).
                let mut revived = n.clone();
                revived.status = "pending".into();
                t.put(&NodeKey { id: revived.id }, &revived)?;
                Ok(Verdict::Pending)
            }
            // A fresh claim queues itself; a write the engine refuses is
            // reported, never read as "awaiting approval".
            None => {
                self.create(&Claim {
                    alias: alias.to_string(),
                    credential: credential.to_string(),
                    machine: machine.to_string(),
                    ..Default::default()
                })?;
                Ok(Verdict::Pending)
            }
        }
    }

    /// Create a pending record (the `POST /nodes` surface, and the
    /// handshake's fresh-claim path).
    pub fn create(&self, claim: &Claim) -> anyhow::Result<Node> {
        let alias = claim.alias.as_str();
        let credential = claim.credential.as_str();
        if alias.is_empty() || credential.is_empty() {
            return Err(anyhow!("alias and credential are required"));
        }
        let t = self.table()?;
        // Same (alias, key) already live → idempotent (return the row).
        if let Some(existing) = self
            .rows_of(alias)?
            .into_iter()
            .find(|n| n.credential == credential && n.status != "revoked")
        {
            return Ok(existing);
        }
        let watermark = okm_core::reduce_get::<_, __OkmReduce_Node_0>(
            t.store(),
            <Node as Document>::NS_PREFIX,
            &NodeKey { id: 0 },
            &Node::default(),
        )?
        .unwrap_or(0);
        let row = Node {
            id: watermark + 1,
            alias: alias.to_string(),
            credential: credential.to_string(),
            status: "pending".into(),
            created_at: now_ms(),
            global: 0,
            labels: claim.labels.join("\n"),
            owner: claim.owner.clone(),
            // Empty on purpose, even when the claim arrived from a node that named its box:
            // only an APPROVED registration may write the binding, so a pending claim cannot
            // occupy the machine slot before the operator has decided anything.
            machine: String::new(),
        };
        t.put(&NodeKey { id: row.id }, &row)?;
        Ok(row)
    }

    /// Approve a key for an alias (`POST /admin/nodes/{alias}/approve`).
    /// `credential` Some = that record; None = the sole pending record
    /// (an ambiguous queue is refused, never guessed). Approval rotates:
    /// a previously approved key for the alias falls to `revoked` (§6).
    pub fn approve(&self, alias: &str, credential: Option<&str>) -> anyhow::Result<Node> {
        let t = self.table()?;
        let rows = self.rows_of(alias)?;
        let target = match credential {
            Some(k) => rows
                .iter()
                .find(|n| n.credential == k && n.status != "revoked"),
            None => {
                let mut pending = rows.iter().filter(|n| n.status == "pending");
                let first = pending.next();
                if first.is_none() || pending.next().is_some() {
                    return Err(anyhow!(
                        "no unique pending key for `{alias}` — approve by credential"
                    ));
                }
                first
            }
        }
        .ok_or_else(|| anyhow!("no such record for `{alias}`"))?
        .clone();
        for old in rows
            .iter()
            .filter(|n| n.status == "approved" && n.id != target.id)
        {
            let mut demoted = old.clone();
            demoted.status = "revoked".into();
            t.put(&NodeKey { id: demoted.id }, &demoted)?;
        }
        let mut approved = target;
        approved.status = "approved".into();
        t.put(&NodeKey { id: approved.id }, &approved)?;
        Ok(approved)
    }

    /// Revoke an alias's live records (`DELETE /admin/nodes/{alias}`).
    /// With a key, that record only; without, every live (pending or
    /// approved) record of the alias. Rows survive as `revoked` —
    /// tombstones, not erasure.
    pub fn revoke(&self, alias: &str, credential: Option<&str>) -> anyhow::Result<usize> {
        let t = self.table()?;
        let mut n = 0usize;
        for row in self.rows_of(alias)? {
            let hit = credential.is_none_or(|k| k == row.credential);
            if hit && row.status != "revoked" {
                let mut dead = row;
                dead.status = "revoked".into();
                t.put(&NodeKey { id: dead.id }, &dead)?;
                n += 1;
            }
        }
        if n == 0 {
            return Err(anyhow!("no live record for `{alias}`"));
        }
        Ok(n)
    }

    /// Every record, newest id last (the `GET /admin/nodes` listing;
    /// connection-scale table, a full key scan is the cheap shape).
    pub fn list(&self) -> anyhow::Result<Vec<Node>> {
        let t = self.table()?;
        let mut ids: Vec<u64> = t
            .scan_keys(&okm_core::PrefixProbe::empty())
            .map(|r| r.map(|k| k.id))
            .collect::<Result<Vec<_>, _>>()?;
        ids.sort();
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = t.get_row(&NodeKey { id })? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    fn rows_of(&self, alias: &str) -> anyhow::Result<Vec<Node>> {
        let t = self.table()?;
        let mut rows = Vec::new();
        for hit in t.scan::<NodeByAlias>(alias.as_bytes()) {
            let (_, _, row) = hit?;
            if let Some(n) = row.filter(|n| n.alias == alias) {
                rows.push(n);
            }
        }
        Ok(rows)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> NodeStore {
        let dir = tempfile::tempdir().unwrap();
        // Registry owns the directory lock; the node store shares it.
        let r = crate::identity::Registry::open(&dir.keep()).unwrap();
        r.nodes()
    }

    fn registered(v: Verdict) -> bool {
        matches!(v, Verdict::Registered { .. })
    }

    #[test]
    fn verdict_lifecycle() {
        let s = store();
        // Fresh claim → pending + a visible row.
        assert_eq!(
            s.verdict("home-pc", "KEY-A", "box-1").unwrap(),
            Verdict::Pending
        );
        assert_eq!(s.list().unwrap().len(), 1);
        // Retry is idempotent pending (same row, no duplicate).
        assert_eq!(
            s.verdict("home-pc", "KEY-A", "box-1").unwrap(),
            Verdict::Pending
        );
        assert_eq!(s.list().unwrap().len(), 1);
        // Approve by alias (unique pending) → registered.
        let approved = s.approve("home-pc", None).unwrap();
        assert_eq!(approved.credential, "KEY-A");
        assert!(registered(s.verdict("home-pc", "KEY-A", "box-1").unwrap()));
        // A different key under the alias → conflict, never takeover.
        assert!(matches!(
            s.verdict("home-pc", "KEY-B", "box-2").unwrap(),
            Verdict::Conflict { .. }
        ));
        assert_eq!(
            s.list()
                .unwrap()
                .iter()
                .find(|n| n.credential == "KEY-B")
                .unwrap()
                .status,
            "pending",
            "the conflicting claim is still queued for the operator"
        );
        // Rotation: approve KEY-B → A demotes to revoked (§6).
        s.approve("home-pc", Some("KEY-B")).unwrap();
        assert!(registered(s.verdict("home-pc", "KEY-B", "box-2").unwrap()));
        assert!(matches!(
            s.verdict("home-pc", "KEY-A", "box-1").unwrap(),
            Verdict::Conflict { .. }
        ));
        // Revocation: B falls to revoked; a revoked key re-claiming is
        // back to pending, never self-service (the revive rewrote B).
        s.revoke("home-pc", None).unwrap();
        assert_eq!(
            s.verdict("home-pc", "KEY-B", "box-2").unwrap(),
            Verdict::Pending
        );
        assert_eq!(
            s.list()
                .unwrap()
                .iter()
                .filter(|n| n.status == "pending")
                .count(),
            1,
            "only B revived"
        );
        assert_eq!(
            s.list()
                .unwrap()
                .iter()
                .filter(|n| n.status == "revoked")
                .count(),
            1,
            "A stays a tombstone"
        );
    }

    #[test]
    fn a_token_is_bound_to_the_first_machine_that_presents_it() {
        let s = store();
        // Minted by the operator: a name the humans use, and whose it is.
        let row = s
            .create(&Claim {
                alias: "laptop".into(),
                credential: "KEY".into(),
                machine: String::new(),
                labels: vec!["MyLaptop".into()],
                owner: "u1".into(),
            })
            .unwrap();
        assert_eq!(row.status, "pending", "minting queues; it does not enrol");
        s.approve("laptop", None).unwrap();

        // First approved registration BINDS the token to the hardware that presented it.
        match s.verdict("laptop", "KEY", "box-7").unwrap() {
            Verdict::Registered { labels, owner } => {
                assert_eq!(labels, vec!["MyLaptop".to_string()]);
                assert_eq!(owner, "u1");
            }
            other => panic!("expected registered, got {other:?}"),
        }
        assert_eq!(
            s.list().unwrap()[0].machine,
            "box-7",
            "the box claimed the token"
        );

        // The same machine reconnecting is the same node.
        assert!(registered(s.verdict("laptop", "KEY", "box-7").unwrap()));

        // A different machine presenting the same token: refused, and the reason names both
        // sides — "one token, one use" is a fact about hardware, not a guess.
        match s.verdict("laptop", "KEY", "box-9").unwrap() {
            Verdict::Conflict { why } => {
                assert!(why.contains("box-7"), "{why}");
                assert!(why.contains("box-9"), "{why}");
            }
            other => panic!("expected conflict, got {other:?}"),
        }
    }

    #[test]
    fn a_node_that_cannot_name_its_hardware_stays_unbound() {
        let s = store();
        s.create(&Claim {
            alias: "anon".into(),
            credential: "KEY".into(),
            ..Default::default()
        })
        .unwrap();
        s.approve("anon", None).unwrap();
        // No machine to bind: the token works, and the registry says out loud that it is not
        // confined to one piece of hardware rather than inventing an identity for it.
        assert!(registered(s.verdict("anon", "KEY", "").unwrap()));
        assert!(s.list().unwrap()[0].machine.is_empty());
        assert!(registered(s.verdict("anon", "KEY", "box-later").unwrap()));
        assert_eq!(
            s.list().unwrap()[0].machine,
            "box-later",
            "an unidentified first registration binds nothing; the first identified one does"
        );
    }

    #[test]
    fn approve_is_never_a_guess() {
        let s = store();
        s.verdict("two-pending", "K1", "box-a").unwrap();
        s.verdict("two-pending", "K2", "box-b").unwrap();
        // Two pending, keyless approve → refused (the honest-cost
        // comparison is the operator's job).
        assert!(s.approve("two-pending", None).is_err());
        assert!(s.approve("two-pending", Some("K1")).is_ok());
        assert!(s.approve("no-such-alias", None).is_err());
    }

    #[test]
    fn posture_parse_is_closed() {
        assert_eq!(Posture::parse("required").unwrap(), Posture::Required);
        assert_eq!(Posture::parse("open").unwrap(), Posture::Open);
        assert!(Posture::parse("").is_err());
        assert!(Posture::parse("trust-me").is_err());
    }
}
