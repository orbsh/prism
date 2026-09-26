//! identity — ADR-0017 §2/§3/§7: the account registry, device anchors,
//! and the per-connection auth state, on prism's OWN okm instance.
//!
//! Placement (the 2026-09-22 amendment carried in §7): the account
//! registry, device↔user bindings, and login live in PRISM's storage —
//! a separate okm instance (its own directory), never in aura's meta or
//! data plane. Aura receives the authenticated sender as payload
//! metadata only.
//!
//! Identity model, exactly two kinds (§2):
//! - `device_id` — assigned on first connect (u64 from the Device
//!   watermark), returned in the `connected` frame; a reconnect echoes
//!   it via `?device=<id>` and the user binding restores (the Device
//!   row's `user_id` is the durable half; the live auth field is the
//!   per-connection half — "the binding persists across reconnects").
//! - `user_id` — created by `signup`, restored by `login` (username +
//!   password verified against the argon2id hash).
//!
//! Key shapes follow the family's registry pattern (aura's
//! realm/src/meta.rs precedent): fixed-width proxy id key + payload
//! mirror + `by_name` text index + `HighWater` reduce. The username is
//! open-ended runtime DATA — it resolves through the index, never as a
//! key.
//!
//! One connection set (§3, revised): auth state is a per-connection
//! field in the gateway's table, not a second collection — a login
//! writes one place, drift between two sets is unrepresentable, and
//! fan-out filters by the field (the honest cost, recorded in the ADR).

use anyhow::anyhow;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use okm_core::document::Collection;
use okm_core::{Document, DocumentEncode, KeyEncode, ReduceCodec, ReduceLogic};

/// The account table (prism instance, ns 50). Password hash is an
/// argon2id PHC string (salt embedded) — never plaintext, never a
/// home-grown hash.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct AccountKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(AccountKey)]
#[ok_index(by_name { fields(username) })]
#[ok_reduce(HighWater(id) { group(global) })]
#[ok_ns(50)]
pub struct Account {
    /// Payload mirror of the proxy id (the TypeName precedent: fold
    /// stays payload-shaped, the row stays self-describing).
    pub id: u64,
    pub username: String,
    pub nickname: String,
    pub password_hash: String,
    /// Single-group discriminator for the registry-wide watermark
    /// (always 0 — the derive rejects empty group lists).
    pub global: u32,
}

use __OkmIndex_Account_by_name as AccountByName;

/// The device table (prism instance, ns 51): id → bound user
/// (0 = still anonymous).
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct DeviceKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DeviceKey)]
#[ok_reduce(HighWater(id) { group(global) })]
#[ok_ns(51)]
pub struct Device {
    pub id: u64,
    pub user_id: u64,
    pub global: u32,
}

/// prism's own okm keyspace. `FjallStore` clones are shared handles
/// (Arc-inner, okm's own comment), so assembly per operation is cheap
/// and concurrent — same shape aura's meta plane runs.
#[derive(Clone)]
pub struct Registry {
    store: okm_core::FjallStore,
}

impl Registry {
    /// Persistent instance: one okm keyspace inside a fjall database at
    /// `dir` (fjall locks the directory per process — one open, ever).
    pub fn open(dir: &std::path::Path) -> anyhow::Result<Self> {
        let store = okm_core::FjallStore::open(dir, "prism")
            .map_err(|e| anyhow!("prism registry open {dir:?}: {e}"))?;
        Ok(Self { store })
    }

    /// The node registry view of the SAME instance (ADR-0015 Phase
    /// 1.8): one directory, one handle, one process ever opens it.
    pub fn nodes(&self) -> crate::nodes::NodeStore {
        crate::nodes::NodeStore::new(self.store.clone())
    }

    /// First connect (or unknown-`?device=` echo): mint a device id
    /// from the watermark, persist the anonymous row.
    pub fn new_device(&self) -> anyhow::Result<u64> {
        let mut t = Collection::<_, DeviceKey, Device>::new(self.store.clone());
        let watermark = okm_core::reduce_get::<_, __OkmReduce_Device_0>(
            t.store(),
            <Device as Document>::NS_PREFIX,
            &DeviceKey { id: 0 },
            &Device { id: 0, user_id: 0, global: 0 },
        )
        .unwrap_or(0);
        let id = watermark + 1;
        t.put(&DeviceKey { id }, &Device { id, user_id: 0, global: 0 });
        Ok(id)
    }

    /// A `?device=<id>` echo is honored only for a real row; anything
    /// else is a fresh connect (an attacker's guessed id mints a new
    /// row, gaining nothing).
    pub fn device_user(&self, id: u64) -> Option<u64> {
        let t = Collection::<_, DeviceKey, Device>::new(self.store.clone());
        t.get(&DeviceKey { id }).filter(|d| d.user_id != 0).map(|d| d.user_id)
    }

    /// signup: create the account and bind the device in one pass.
    /// Uniqueness = by_name index scan + row verify (exact match over
    /// the text index; a prefix collision reads as not-taken only if
    /// the FULL username differs — the scan verifies it).
    pub fn signup(
        &self,
        username: &str,
        nickname: &str,
        password: &str,
        device_id: u64,
    ) -> anyhow::Result<u64> {
        if username.is_empty() || password.is_empty() {
            return Err(anyhow!("username and password are required"));
        }
        let hash = hash_password(password)?;
        let mut t = Collection::<_, AccountKey, Account>::new(self.store.clone());
        if self.find_account(username).is_some() {
            return Err(anyhow!("username taken"));
        }
        let watermark = okm_core::reduce_get::<_, __OkmReduce_Account_0>(
            t.store(),
            <Account as Document>::NS_PREFIX,
            &AccountKey { id: 0 },
            &Account {
                id: 0,
                username: String::new(),
                nickname: String::new(),
                password_hash: String::new(),
                global: 0,
            },
        )
        .unwrap_or(0);
        let id = watermark + 1;
        t.put(
            &AccountKey { id },
            &Account {
                id,
                username: username.to_string(),
                nickname: nickname.to_string(),
                password_hash: hash,
                global: 0,
            },
        );
        self.bind_device(device_id, id);
        Ok(id)
    }

    /// login: resolve username → account, verify the hash, bind the
    /// device, return the user id. Unknown name and wrong password
    /// share one error string — the gateway does not distinguish (user
    /// enumeration is deployment's problem, not this table's).
    pub fn login(&self, username: &str, password: &str, device_id: u64) -> anyhow::Result<u64> {
        let account = self.find_account(username).ok_or_else(|| anyhow!("invalid credentials"))?;
        verify_password(password, &account.password_hash)?;
        self.bind_device(device_id, account.id);
        Ok(account.id)
    }

    fn find_account(&self, username: &str) -> Option<Account> {
        let t = Collection::<_, AccountKey, Account>::new(self.store.clone());
        t.scan::<AccountByName>(username.as_bytes())
            .into_iter()
            .find_map(|h| h.1.filter(|a| a.username == username))
    }

    fn bind_device(&self, device_id: u64, user_id: u64) {
        let mut t = Collection::<_, DeviceKey, Device>::new(self.store.clone());
        if let Some(mut d) = t.get(&DeviceKey { id: device_id }) {
            d.user_id = user_id;
            t.put(&DeviceKey { id: device_id }, &d);
        }
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

    /// Registry-level shape locks (the e2e covers the wire; these cover
    /// the storage logic independent of a socket): watermark ids,
    /// uniqueness, enumeration-free failure, the device binding.
    #[test]
    fn registry_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let r = Registry::open(&dir.into_path()).unwrap();

        let alice = r.signup("alice", "Ali", "pw-a", 0).unwrap();
        let bob = r.signup("bob", "Bo", "pw-b", 0).unwrap();
        assert_eq!((alice, bob), (1, 2), "watermark ids, never reused");
        assert!(r.signup("alice", "", "other", 0).is_err(), "username unique");

        // enumeration-free: unknown name and wrong password share one error
        let e1 = r.login("alice", "nope", 0).unwrap_err().to_string();
        let e2 = r.login("ghost", "nope", 0).unwrap_err().to_string();
        assert_eq!(e1, e2);

        // device binding: unknown device -> None; after login the row
        // carries the user (the reconnect restore source).
        assert_eq!(r.device_user(9), None, "unknown device");
        let d = r.new_device().unwrap();
        let uid = r.login("alice", "pw-a", d).unwrap();
        assert_eq!(uid, alice);
        assert_eq!(r.device_user(d), Some(alice), "binding durable");
    }
}
