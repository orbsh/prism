//! The auth booth: the half of authentication a deployment may replace.
//!
//! prism keeps the verbs (`login` / `signup` / `logout`), the device↔user binding, the token and the
//! idle clock. What is NOT fixed is how a credential is proven and what the login surface looks like
//! — that is this booth's business. The seam is ONE CALL (`op: "verify"` / `op: "register"`), never
//! an event: every emit is persisted as an MQ row (aura, `realm/src/events.rs`) and a payload that
//! reaches no route is served by the venue's unauthenticated `/admin/dead`, so a password must never
//! travel that way. A call writes no queue row.
//!
//! The shipped implementation keeps two tables in prism's own okm instance:
//!
//! - `Account` (ns 50) — username + nickname, with `by_name` for the exact login lookup;
//! - `Credential` (ns 55) — the argon2id verifier, keyed by USER ID.
//!
//! They are two tables because a verifier is not profile data: a listing, a dump or an export of
//! accounts must not carry something that can be cracked offline. A deployment that swaps this booth
//! (an external IdP, an LDAP bind, a redirect flow) replaces this file's policy, and `identity.rs`
//! does not move: it only ever receives the user id the booth named.

use anyhow::anyhow;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use aura_booth::{Body, BoothType, Concurrency};
use aura_engine::Engine;
use okm_core::document::Collection;
use okm_core::{DocumentEncode, FjallStore, Uuid7};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::identity::UserId;

/// The booth type the venue calls. The NAME is fixed — `auth` is a platform word, not a business one —
/// and a deployment swaps the IMPLEMENTATION by registering another type under it.
pub const AUTH_BOOTH: &str = "auth";
/// The operations. They ride the call's payload because a Rust body is not told which handler the
/// call named (aura dispatches `Body::Rust` with `(ctx, args)` only) — the same reason the plane's
/// envelope carries `name`.
pub const OP_VERIFY: &str = "verify";
pub const OP_REGISTER: &str = "register";

/// The account table (ns 50): who a user id IS, for display and for the login lookup. No verifier
/// here — see `Credential`.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(Uuid7)]
#[ok_index(by_name { fields(username) })]
#[ok_ns(50)]
pub struct Account {
    pub username: String,
    pub nickname: String,
}

use __OkmIndex_Account_by_name as AccountByName;

/// The verifier table (ns 55), keyed by user id: an argon2id PHC string (salt embedded) — never
/// plaintext, never a home-grown hash. Its own row so that reading accounts never reads hashes.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(Uuid7)]
#[ok_ns(55)]
pub struct Credential {
    pub hash: String,
}

/// The shipped booth's storage: the two tables above, over prism's own okm instance. A different
/// implementation answers the same call and stores wherever it likes.
#[derive(Clone)]
pub struct AuthStore {
    store: FjallStore,
}

impl AuthStore {
    pub fn new(store: FjallStore) -> Self {
        Self { store }
    }

    /// Create the account and its verifier; returns the user id the venue will bind. The verifier is
    /// written FIRST: a half-finished registration then leaves an orphan hash rather than an account
    /// whose name is taken and which nobody can ever log into.
    pub fn register(&self, username: &str, nickname: &str, password: &str) -> anyhow::Result<UserId> {
        if username.is_empty() || password.is_empty() {
            return Err(anyhow!("username and password are required"));
        }
        if self.find_account(username)?.is_some() {
            return Err(anyhow!("username taken"));
        }
        let id = Uuid7::now();
        let creds = Collection::<_, UserId, Credential>::new(self.store.clone());
        creds.put(&id, &Credential { hash: hash_password(password)? })?;
        let accounts = Collection::<_, UserId, Account>::new(self.store.clone());
        accounts.put(
            &id,
            &Account { username: username.to_string(), nickname: nickname.to_string() },
        )?;
        Ok(id)
    }

    /// Prove a credential and name the user. An unknown username and a wrong password answer the SAME
    /// error: the venue does not turn this door into an account directory.
    pub fn verify(&self, username: &str, password: &str) -> anyhow::Result<UserId> {
        let (user, _) = self
            .find_account(username)?
            .ok_or_else(|| anyhow!("invalid credentials"))?;
        let cred = Collection::<_, UserId, Credential>::new(self.store.clone())
            .get(&user)?
            .ok_or_else(|| anyhow!("invalid credentials"))?;
        verify_password(password, &cred.hash)?;
        Ok(user)
    }

    /// Username → (id, row). The text lives in the index; the row verifies an exact match, because an
    /// index hit may be a prefix collision.
    pub fn find_account(&self, username: &str) -> anyhow::Result<Option<(UserId, Account)>> {
        let t = Collection::<_, UserId, Account>::new(self.store.clone());
        Ok(t.scan::<AccountByName>(username.as_bytes())?
            .into_iter()
            .find_map(|(key, row)| row.filter(|a| a.username == username).map(|a| (key.decoded, a))))
    }
}

/// Register the shipped booth. A deployment that wants another implementation registers its own
/// `auth` type instead — and a venue with no `auth` type at all is legal: it serves its anonymous
/// plane and answers every login attempt with the standard error naming the missing booth.
pub async fn install(engine: &Engine, store: FjallStore) -> anyhow::Result<()> {
    let booth = BoothType {
        name: AUTH_BOOTH.to_string(),
        body: Body::Rust(Arc::new(move |_ctx, args| {
            let store = store.clone();
            Box::pin(async move { handle(store, args).await })
        })),
        idle_ttl: None,
        max_exec: None,
        hooks: std::collections::BTreeMap::new(),
        // A call target needs no subscription: the venue is the only caller, and it calls.
        receives: Vec::new(),
        concurrency: Concurrency::Queued,
    };
    engine.register(booth).await
}

/// One call: the operation names the policy, the payload carries what it needs. Hashing is CPU work
/// (argon2id's default parameters), so it runs off the async worker.
async fn handle(store: FjallStore, args: Value) -> anyhow::Result<Value> {
    let op = args.get("op").and_then(Value::as_str).unwrap_or("");
    let field = |name: &str| -> anyhow::Result<String> {
        args.get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("auth `{op}` needs `{name}`"))
    };
    let store = AuthStore::new(store);
    match op {
        OP_VERIFY => {
            let (username, password) = (field("username")?, field("password")?);
            let user = tokio::task::spawn_blocking(move || store.verify(&username, &password)).await??;
            Ok(json!({ "user": user.to_string() }))
        }
        OP_REGISTER => {
            let (username, password) = (field("username")?, field("password")?);
            let nickname = args.get("nickname").and_then(Value::as_str).unwrap_or("").to_string();
            let user = tokio::task::spawn_blocking(move || {
                store.register(&username, &nickname, &password)
            })
            .await??;
            Ok(json!({ "user": user.to_string() }))
        }
        other => Err(anyhow!("unknown auth operation `{other}`")),
    }
}

fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("hash: {e}"))
}

fn verify_password(password: &str, hash: &str) -> anyhow::Result<()> {
    let parsed = PasswordHash::new(hash).map_err(|e| anyhow!("stored hash: {e}"))?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| anyhow!("invalid credentials"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> AuthStore {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir: fjall locks the directory, and the guard would unlink it under the db.
        AuthStore::new(FjallStore::open(&dir.keep(), "prism").unwrap())
    }

    /// The whole point of the split: a credential proves a user, and the same row is what the venue
    /// binds — no password ever leaves this module.
    #[test]
    fn a_credential_names_a_user_and_the_verifier_has_its_own_row() {
        let s = store();
        let user = s.register("alice", "Ali", "pw-a").unwrap();
        assert_eq!(s.verify("alice", "pw-a").unwrap(), user, "the same user id comes back");
        assert_eq!(s.find_account("alice").unwrap().unwrap().1.nickname, "Ali");

        // The verifier is a row of its own, keyed by the USER ID — not a field of the account.
        let creds = Collection::<_, UserId, Credential>::new(s.store.clone());
        assert!(creds.get(&user).unwrap().is_some(), "ns 55 holds the hash for this user");
    }

    /// One error for both halves of a failed login, and a taken username is refused.
    #[test]
    fn a_wrong_password_and_an_unknown_name_read_the_same() {
        let s = store();
        s.register("alice", "Ali", "pw-a").unwrap();
        let wrong = s.verify("alice", "nope").unwrap_err().to_string();
        let ghost = s.verify("ghost", "nope").unwrap_err().to_string();
        assert_eq!(wrong, ghost, "enumeration-free");
        assert!(s.register("alice", "", "other").is_err(), "username is unique");
    }
}