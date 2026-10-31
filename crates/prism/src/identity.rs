//! identity — ADR-0017 §2/§3/§7 and the 2026-10-29 split: accounts, devices, and the token that
//! is the ONLY credential.
//!
//! Three collections in prism's OWN okm instance (never aura's meta or data plane):
//!
//! | ns | key | payload |
//! |---|---|---|
//! | 50 `Account` | `Uuid7` | username, nickname, argon2id PHC hash |
//! | 51 `Device`  | `Uuid7` | user, alias, first_seen, revoked |
//! | 53 `Token`   | `Uuid4` | device, user, issued_at, revoked_at |
//! | 54 `Activity`| `Uuid7` | at — the clock the idle window reads |
//!
//! (ns 52 is the effector node registry — see `nodes.rs`.)
//!
//! The keys are `okm_core::Uuid7`/`Uuid4`: 16 raw bytes on the wire, no text anywhere in the
//! store. Two consequences the shape relies on:
//!
//! - **No payload mirror.** `get`/`scan` hand the key back, so a row does not repeat its own name
//!   and there is no key-vs-payload state that only discipline can hold.
//! - **Which uuid is a storage question.** `Uuid7` (millisecond prefix, byte order = time order)
//!   for ids inserted in sequence — devices, accounts. `Uuid4` (128 random bits) for the token,
//!   which is a credential: it wants unpredictability and has no order to keep.
//!
//! The relationship, one line each:
//!
//! - a **device** is a stable server-issued id plus a user-editable ALIAS; the UI shows the alias.
//!   It is never fingerprinted on the client — a browser cannot see hardware, and a canvas hash
//!   collides, which is a broken identity rather than a duplicate row.
//! - a **token** is what authenticates: server-issued, revocable, and it names its device. `?device=`
//!   alone restores nothing, which is the hole this split closes. It has no expiry of its own: the
//!   clock is the device's last activity (ns 54), so a credential whose device has been idle past
//!   the window restores nothing either — and the device is still named, which is what lets a login
//!   keep its alias instead of minting a new device every timeout.
//! - an **account** is a unique username + password hash; one user, many devices.
//!
//! Live auth state stays where it was (§3): a per-connection field in the gateway, never a second
//! collection. `resolve_token` is the only thing that fills it.

use anyhow::anyhow;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use okm_core::document::Collection;
use okm_core::{DocumentEncode, KeyEncode, Uuid4, Uuid7};

/// A device id: what a token resolves to, what a connection carries, what a UI shows as an alias.
pub type DeviceId = Uuid7;
/// A user id. The nil uuid means "anonymous" — the sentinel the old `0` was.
pub type UserId = Uuid7;
/// A token id: the credential itself, not a handle to one.
pub type TokenId = Uuid4;

/// The anonymous sentinel (nil uuid), spelled once so nobody hand-rolls the test.
pub fn anonymous() -> UserId {
    Uuid7::from_bytes([0u8; 16])
}

/// Is this the anonymous sentinel?
pub fn is_anonymous(user: UserId) -> bool {
    user == anonymous()
}

/// The account table (prism instance, ns 50). The key is the user id; the password hash is an
/// argon2id PHC string (salt embedded) — never plaintext, never a home-grown hash.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(Uuid7)]
#[ok_index(by_name { fields(username) })]
#[ok_ns(50)]
pub struct Account {
    pub username: String,
    pub nickname: String,
    pub password_hash: String,
}

use __OkmIndex_Account_by_name as AccountByName;

/// The device table (prism instance, ns 51): a stable id, its binding, and the name the user gave
/// it. `revoked` is the server's stamp (0 = never): a revoked device restores nothing until fresh
/// credentials re-earn it.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(Uuid7)]
#[ok_index(by_user { fields(user) })]
#[ok_ns(51)]
pub struct Device {
    /// [`anonymous()`] until a login/signup binds it.
    pub user: Uuid7,
    /// The user's name for this device. Empty means "show the id".
    /// (The fields name the concrete scalars because the derive matches on the type's SPELLING;
    /// the `DeviceId`/`UserId` aliases are for signatures and read better there.)
    pub alias: String,
    pub first_seen: u64,
    pub revoked: u64,
}

use __OkmIndex_Device_by_user as DeviceByUser;

/// The token table (prism instance, ns 53): the credential, keyed by itself. It carries the
/// (device, user) pair so resolving needs no second lookup, and an index by device so logout and
/// rotation revoke the SET without a scan.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(Uuid4)]
#[ok_index(by_device { fields(device) })]
#[ok_ns(53)]
pub struct Token {
    pub device: Uuid7,
    pub user: Uuid7,
    pub issued_at: u64,
    /// Rotation, logout and revocation all write here; 0 = live.
    pub revoked_at: u64,
}

use __OkmIndex_Token_by_device as TokenByDevice;

/// The activity table (prism instance, ns 54): the clock, one scalar per device. It is a table of
/// its own because it is written on the MESSAGE path — a read-modify-write of the device row per
/// message would race the alias and the binding (two writers, one row, one lost update). The device
/// row keeps what changes rarely; this keeps what changes constantly.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(Uuid7)]
#[ok_ns(54)]
pub struct Activity {
    pub at: u64,
}

/// What one connect resolved to: the device it addresses, the user it may act as, the credential the
/// page keeps, and whether the device had been silent past the window. When `idle` is true the
/// connection is NOT authenticated — the device is still named (its alias survives a timeout), and a
/// login on this same connection is what re-earns the trust.
pub struct Identified {
    pub device: DeviceId,
    pub user: UserId,
    pub token: TokenId,
    pub idle: bool,
}

/// How long a device may stay silent before its connection stops being authenticated: 30 days.
/// `PRISM_IDLE_WINDOW` (seconds) overrides. One clock for the whole identity plane — the token has
/// no expiry, because two clocks for one question is two answers waiting to disagree.
const DEFAULT_IDLE_SECS: u64 = 30 * 24 * 60 * 60;

/// prism's own okm keyspace. `FjallStore` clones are shared handles, so assembly per operation is
/// cheap and concurrent — the same shape aura's meta plane runs.
#[derive(Clone)]
pub struct Registry {
    store: okm_core::FjallStore,
    idle_ms: u64,
}

impl Registry {
    /// Persistent instance: one okm keyspace inside a fjall database at `dir` (fjall locks the
    /// directory per process — one open, ever).
    pub fn open(dir: &std::path::Path) -> anyhow::Result<Self> {
        Self::open_with_idle(dir, idle_from_env())
    }

    /// The same instance with the window spelled out — one deployment can want a week and a test a
    /// hundred milliseconds, and neither should have to set an environment variable to say so.
    pub fn open_with_idle(dir: &std::path::Path, idle_ms: u64) -> anyhow::Result<Self> {
        let store = okm_core::FjallStore::open(dir, "prism")
            .map_err(|e| anyhow!("prism registry open {dir:?}: {e}"))?;
        Ok(Self { store, idle_ms })
    }

    /// The node registry view of the SAME instance (ns 52, ADR-0015 Phase 1.8): one directory, one
    /// handle, one process ever opens it.
    pub fn nodes(&self) -> crate::nodes::NodeStore {
        crate::nodes::NodeStore::new(self.store.clone())
    }

    /// Mint a device: a fresh uuid7 row, anonymous, no alias. Nothing is fingerprint-based, so two
    /// browsers that look identical still get two ids.
    pub fn new_device(&self) -> anyhow::Result<DeviceId> {
        let t = Collection::<_, DeviceId, Device>::new(self.store.clone());
        let id = Uuid7::now();
        let now = now_ms();
        t.put(
            &id,
            &Device {
                user: anonymous(),
                alias: String::new(),
                first_seen: now,
                revoked: 0,
            },
        )?;
        self.touch_at(id, now)?;
        Ok(id)
    }

    /// One device row, if it exists.
    pub fn device(&self, id: DeviceId) -> anyhow::Result<Option<Device>> {
        let t = Collection::<_, DeviceId, Device>::new(self.store.clone());
        Ok(t.get(&id)?)
    }

    /// The binding a device names, or None when the row is anonymous or revoked. This is the
    /// restore path; nothing else may mint an authenticated connection.
    pub fn device_user(&self, id: DeviceId) -> anyhow::Result<Option<UserId>> {
        Ok(self
            .device(id)?
            .filter(|d| !is_anonymous(d.user) && d.revoked == 0)
            .map(|d| d.user))
    }

    /// Rename a device. The alias is display only — it never participates in a decision, so two
    /// devices may share one.
    pub fn set_alias(&self, id: DeviceId, alias: &str) -> anyhow::Result<()> {
        let t = Collection::<_, DeviceId, Device>::new(self.store.clone());
        let Some(mut d) = t.get(&id)? else {
            return Err(anyhow!("unknown device {}", id));
        };
        d.alias = alias.to_string();
        t.put(&id, &d)?;
        self.touch(id)?;
        Ok(())
    }

    /// signup: create the account, bind the device, hand back the first token. The token is what
    /// the page keeps; the user id is what the connection's auth field takes.
    pub fn signup(
        &self,
        username: &str,
        nickname: &str,
        password: &str,
        device: DeviceId,
    ) -> anyhow::Result<(UserId, TokenId)> {
        if username.is_empty() || password.is_empty() {
            return Err(anyhow!("username and password are required"));
        }
        if self.find_account(username)?.is_some() {
            return Err(anyhow!("username taken"));
        }
        let hash = hash_password(password)?;
        let id = Uuid7::now();
        let t = Collection::<_, UserId, Account>::new(self.store.clone());
        t.put(
            &id,
            &Account {
                username: username.to_string(),
                nickname: nickname.to_string(),
                password_hash: hash,
            },
        )?;
        self.bind_device(device, id)?;
        Ok((id, self.issue_token(device, id)?))
    }

    /// login: resolve username → account, verify the hash, bind the device, rotate the token.
    /// Unknown name and wrong password share one error string — the gateway does not distinguish
    /// (user enumeration is deployment's problem, not this table's).
    pub fn login(
        &self,
        username: &str,
        password: &str,
        device: DeviceId,
    ) -> anyhow::Result<(UserId, TokenId)> {
        let (user, account) =
            self.find_account(username)?.ok_or_else(|| anyhow!("invalid credentials"))?;
        verify_password(password, &account.password_hash)?;
        self.bind_device(device, user)?;
        Ok((user, self.issue_token(device, user)?))
    }

    /// logout: revoke every token of the device and clear its binding. Idempotent by construction.
    /// The gateway separately clears its per-connection auth field; the two facts are independent
    /// (§3: the field drives fan-out, the table survives reconnects).
    pub fn logout(&self, device: DeviceId) -> anyhow::Result<()> {
        self.revoke_tokens_of(device)?;
        self.bind_device(device, anonymous())
    }

    /// Server-side revocation: the binding clears AND the trust dies, and no token of that device
    /// survives — re-login is the only path back. Returns the prior binding (the caller reports
    /// it; anonymous = was anonymous).
    pub fn revoke_device(&self, device: DeviceId) -> anyhow::Result<UserId> {
        let t = Collection::<_, DeviceId, Device>::new(self.store.clone());
        let Some(mut d) = t.get(&device)? else {
            return Err(anyhow!("unknown device {}", device));
        };
        let was = d.user;
        d.user = anonymous();
        d.revoked = now_ms();
        t.put(&device, &d)?;
        self.revoke_tokens_of(device)?;
        Ok(was)
    }

    /// Issue a token for a device/user pair, revoking the device's older ones first: a login is a
    /// ROTATION, so a leaked token does not survive the next login.
    pub fn issue_token(&self, device: DeviceId, user: UserId) -> anyhow::Result<TokenId> {
        self.revoke_tokens_of(device)?;
        let t = Collection::<_, TokenId, Token>::new(self.store.clone());
        let id = Uuid4::new();
        let now = now_ms();
        t.put(
            &id,
            &Token {
                device,
                user,
                issued_at: now,
                revoked_at: 0,
            },
        )?;
        Ok(id)
    }

    /// Resolve a presented token: `Some((device, user))` when it is live. Freshness is NOT decided
    /// here — the credential names who, [`Registry::idle`] decides whether that name still carries
    /// permission. A revoked row reads None and is left alone.
    pub fn resolve_token(&self, token: TokenId) -> anyhow::Result<Option<(DeviceId, UserId)>> {
        let t = Collection::<_, TokenId, Token>::new(self.store.clone());
        let Some(row) = t.get(&token)? else {
            return Ok(None);
        };
        if row.revoked_at != 0 {
            return Ok(None);
        }
        // The device must still agree: a revoked device invalidates its tokens without touching
        // every row (the binding is the authority for trust, the token for freshness).
        if !self.device_user(row.device)?.is_some_and(|u| u == row.user) {
            return Ok(None);
        }
        Ok(Some((row.device, row.user)))
    }

    /// Revoke one token (a single tab's logout, or an operator's kick). Reports whether it flipped.
    pub fn revoke_token(&self, token: TokenId) -> anyhow::Result<bool> {
        let t = Collection::<_, TokenId, Token>::new(self.store.clone());
        let Some(mut row) = t.get(&token)? else {
            return Ok(false);
        };
        if row.revoked_at != 0 {
            return Ok(false);
        }
        row.revoked_at = now_ms();
        t.put(&token, &row)?;
        Ok(true)
    }

    /// Revoke every token of a device — rotation, logout, device revocation. Returns how many.
    pub fn revoke_tokens_of(&self, device: DeviceId) -> anyhow::Result<usize> {
        let t = Collection::<_, TokenId, Token>::new(self.store.clone());
        let mut n = 0usize;
        let now = now_ms();
        for hit in t.scan::<TokenByDevice>(device.encode().as_slice())? {
            let (key, Some(mut row)) = (hit.0, hit.1) else {
                continue;
            };
            if row.revoked_at != 0 || row.device != device {
                continue;
            }
            row.revoked_at = now;
            t.put(&key.decoded, &row)?;
            n += 1;
        }
        Ok(n)
    }

    /// Every device bound to a user, via the `by_user` index. This is what a "my devices" list
    /// reads: ids are uuid7 (clock-ordered, so the list arrives in the order they were first seen)
    /// and the alias is what the page shows.
    pub fn devices_of(&self, user: UserId) -> anyhow::Result<Vec<(DeviceId, Device)>> {
        let t = Collection::<_, DeviceId, Device>::new(self.store.clone());
        let mut out = Vec::new();
        for hit in t.scan::<DeviceByUser>(user.encode().as_slice())? {
            let Some(row) = hit.1 else { continue };
            if row.user == user && row.revoked == 0 {
                out.push((hit.0.decoded, row));
            }
        }
        Ok(out)
    }

    /// Username → (id, row). The text lives in the index; the row verifies an exact match, because
    /// an index hit may be a prefix collision.
    pub fn find_account(&self, username: &str) -> anyhow::Result<Option<(UserId, Account)>> {
        let t = Collection::<_, UserId, Account>::new(self.store.clone());
        Ok(t.scan::<AccountByName>(username.as_bytes())?.into_iter().find_map(
            |(key, row)| row.filter(|a| a.username == username).map(|a| (key.decoded, a)),
        ))
    }

    fn bind_device(&self, device: DeviceId, user: UserId) -> anyhow::Result<()> {
        let t = Collection::<_, DeviceId, Device>::new(self.store.clone());
        if let Some(mut d) = t.get(&device)? {
            d.user = user;
            // A fresh authentication re-earns the device's trust: the revocation stamp clears with
            // the successful bind (the credentials were just proven — the only path back).
            if !is_anonymous(user) {
                d.revoked = 0;
            }
            t.put(&device, &d)?;
        }
        self.touch(device)?;
        Ok(())
    }

    /// The clock: when this device last spoke. `None` when no row exists — a device nobody has heard
    /// from is idle, which is the safe direction (the alternative would be to trust silence).
    pub fn activity(&self, device: DeviceId) -> anyhow::Result<Option<u64>> {
        let t = Collection::<_, DeviceId, Activity>::new(self.store.clone());
        Ok(t.get(&device)?.map(|a| a.at))
    }

    /// Record that the device spoke: one scalar row, no read-modify-write of the device.
    pub fn touch(&self, device: DeviceId) -> anyhow::Result<()> {
        self.touch_at(device, now_ms())
    }

    fn touch_at(&self, device: DeviceId, at: u64) -> anyhow::Result<()> {
        let t = Collection::<_, DeviceId, Activity>::new(self.store.clone());
        t.put(&device, &Activity { at })?;
        Ok(())
    }

    /// Has this device been silent past the window? This is the only place the question is asked, so
    /// there is only one answer: the token does not have a second opinion.
    pub fn idle(&self, device: DeviceId, now: u64) -> anyhow::Result<bool> {
        Ok(match self.activity(device)? {
            None => true,
            Some(at) => now.saturating_sub(at) > self.idle_ms,
        })
    }

    /// The window, for a caller that has to judge several messages against it.
    pub fn idle_ms(&self) -> u64 {
        self.idle_ms
    }
}

fn idle_from_env() -> u64 {
    std::env::var("PRISM_IDLE_WINDOW")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_IDLE_SECS)
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

/// Unix milliseconds. Prism's own clock: the value is this venue's, not an ordering authority.
/// The clock every table in this module stamps with. Public because the venue judges a connection's
/// idleness against the same milliseconds the rows carry.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        // Diagnostic stamp only: a pre-1970 clock reads as the epoch.
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let dir = tempfile::tempdir().unwrap();
        Registry::open(&dir.into_path()).unwrap()
    }

    /// The split, at the storage level: a token authenticates, a device id alone does not, and a
    /// token the table never held is not a credential.
    #[test]
    fn a_token_authenticates_and_a_device_id_alone_does_not() {
        let r = registry();
        let device = r.new_device().unwrap();
        let (alice, token) = r.signup("alice", "Ali", "pw-a", device).unwrap();

        assert_eq!(r.resolve_token(token).unwrap(), Some((device, alice)));
        assert_eq!(r.device_user(device).unwrap(), Some(alice));

        // Ids are uuids: 16 bytes on the wire, and two mints never collide.
        assert_eq!(device.encode().len(), 16);
        assert_ne!(r.new_device().unwrap(), device);

        assert_eq!(r.resolve_token(Uuid4::new()).unwrap(), None, "no row, no session");

        // Logout revokes the token AND clears the binding: the same token stops resolving.
        r.logout(device).unwrap();
        assert_eq!(r.resolve_token(token).unwrap(), None);
        assert_eq!(r.device_user(device).unwrap(), None);
    }

    /// Rotation: a login mints a NEW token and retires the old one, so a leaked token dies at the
    /// next login. Server-side revocation kills the set and the binding.
    #[test]
    fn login_rotates_and_revocation_kills_the_set() {
        let r = registry();
        let device = r.new_device().unwrap();
        let (alice, first) = r.signup("alice", "Ali", "pw-a", device).unwrap();
        let (again, second) = r.login("alice", "pw-a", device).unwrap();
        assert_eq!(alice, again);
        assert_ne!(first, second, "rotation mints a new token");
        assert_eq!(r.resolve_token(first).unwrap(), None, "the old token is retired");
        assert_eq!(r.resolve_token(second).unwrap(), Some((device, alice)));

        assert_eq!(r.revoke_device(device).unwrap(), alice);
        assert_eq!(r.device_user(device).unwrap(), None);
        assert_eq!(r.resolve_token(second).unwrap(), None, "every token of the device died");
        assert!(r.revoke_device(Uuid7::now()).is_err(), "unknown device is a refusal");

        // Re-login re-binds and clears the stamp — the only way back.
        let (_, third) = r.login("alice", "pw-a", device).unwrap();
        assert_eq!(r.device_user(device).unwrap(), Some(alice));
        assert_eq!(r.resolve_token(third).unwrap(), Some((device, alice)));
    }

    /// Accounts: unique username through the text index, one error for both halves of a failed
    /// login, and an alias that is display-only.
    #[test]
    fn accounts_are_unique_and_aliases_are_just_names() {
        let r = registry();
        let a = r.new_device().unwrap();
        let b = r.new_device().unwrap();
        let (alice, _) = r.signup("alice", "Ali", "pw-a", a).unwrap();
        let (bob, _) = r.signup("bob", "Bo", "pw-b", b).unwrap();
        assert_ne!(alice, bob);
        assert!(r.signup("alice", "", "other", b).is_err(), "username unique");

        let e1 = r.login("alice", "nope", a).unwrap_err().to_string();
        let e2 = r.login("ghost", "nope", a).unwrap_err().to_string();
        assert_eq!(e1, e2, "enumeration-free");

        // The "my devices" list reads the by_user index: both rows, newest last.
        let listed = r.devices_of(alice).unwrap();
        assert_eq!(listed.len(), 1, "only alice's device");
        assert_eq!(listed[0].0, a);

        r.set_alias(a, "laptop").unwrap();
        r.set_alias(b, "laptop").unwrap();
        assert_eq!(r.device(a).unwrap().unwrap().alias, "laptop", "aliases may repeat");
        assert_eq!(r.device(b).unwrap().unwrap().alias, "laptop");
        assert!(r.set_alias(Uuid7::now(), "x").is_err(), "unknown device is a refusal");
    }

    /// Freshness is the DEVICE's clock, not the token's: a token that is still live keeps naming its
    /// device after the window has passed — permission is what it loses, and the name is exactly what
    /// a login needs to restore the same device (and its alias) instead of minting a new one.
    #[test]
    fn a_silent_device_is_idle_but_its_token_still_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let r = Registry::open_with_idle(&dir.into_path(), 1).unwrap();
        let device = r.new_device().unwrap();
        let (_, token) = r.signup("alice", "Ali", "pw-a", device).unwrap();
        r.set_alias(device, "laptop").unwrap();
        assert!(!r.idle(device, now_ms()).unwrap(), "a device that just spoke is not idle");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(r.idle(device, now_ms()).unwrap(), "a window of 1ms has passed");
        assert_eq!(
            r.resolve_token(token).unwrap().map(|(d, _)| d),
            Some(device),
            "the credential still says WHICH device"
        );
        // Speaking again ends the idleness, and the alias a login restores is the one it had.
        r.touch(device).unwrap();
        assert!(!r.idle(device, now_ms()).unwrap());
        assert_eq!(r.device(device).unwrap().unwrap().alias, "laptop", "a timeout renames nothing");
        // Logout keeps the id and the name and drops the binding — the page is not re-created.
        r.logout(device).unwrap();
        let row = r.device(device).unwrap().unwrap();
        assert!(is_anonymous(row.user), "logout unbinds");
        assert_eq!(row.alias, "laptop", "logout does not clear the name");
    }
}