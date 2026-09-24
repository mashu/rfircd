//! Registered nicknames. Passwords are stored as Argon2id hashes, never in
//! reversible form. "Encrypted on the server" in the amateur-radio sense:
//! the file is useless without the password, and we cannot recover one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::irc::message::lower;

/// At most this many Argon2id jobs run at once. Each costs 19 MiB; without a
/// process-wide cap, a burst of IDENTIFY from many addresses fills the
/// blocking pool and stalls hashing for everyone, including the operator.
const ARGON2_MAX_IN_FLIGHT: usize = 2;

static ARGON2_SLOTS: Semaphore = Semaphore::const_new(ARGON2_MAX_IN_FLIGHT);

/// A slot `OPER` may use that ordinary account work may not.
///
/// The cap above is process-wide and was shared by IDENTIFY, REGISTER and
/// OPER alike, so enough sources each staying under `identify_per_min` could
/// keep both slots busy indefinitely — and the one person who needs to get in
/// when that is happening is the control operator, to KLINE the source or
/// stop the transmitter. Reserving a slot for them costs 19 MiB and makes
/// `OPER` independent of how much unauthenticated hashing is queued.
static ARGON2_OPER_SLOT: Semaphore = Semaphore::const_new(1);

/// Which queue a password job waits in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WorkClass {
    /// REGISTER, IDENTIFY, UNREGISTER: anyone with a socket can ask.
    Account,
    /// OPER: transmitter control, and the way out of a flood.
    Oper,
}

/// Dummy PHC used when `OPER` names a miss so a hashed `[[opers]]` name is
/// not distinguishable from an unknown name by Argon2 latency. Verification
/// of this string is discarded; the caller always reports a mismatch.
pub(crate) const DUMMY_OPER_PHC: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$zSQjJ545QMpuik2TaPDolQ$n7PzFZZZGHNJDsPNr8OOSMJTE2gU5cOQvk31xduPH60";

/// Argon2id at the OWASP baseline: 19 MiB, two passes, one lane.
///
/// The old 4 MiB setting was well under that, and memory is the parameter
/// that actually costs an offline attacker anything. Hashing happens on a
/// blocking thread pool (see `run_argon2`), never on the event loop, so the
/// cost is paid by the one connection doing REGISTER or IDENTIFY.
///
/// Raising this is backward compatible: a PHC hash string carries the
/// parameters it was made with, and verification uses those, so existing
/// entries in the nick file keep working and are re-hashed at the new cost
/// whenever the password is next set.
fn hasher() -> Argon2<'static> {
    let params = Params::new(19 * 1024, 2, 1, None).expect("static Argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NickAccount {
    pub nick: String,
    pub password_hash: String,
    pub created_unix: u64,
    /// Operator-granted right to have this nick's messages put on the air.
    #[serde(default)]
    pub rf_tx: bool,
    /// Last CALLSIGN this nick claimed. Restored on IDENTIFY; it is still a
    /// claim, not proof of licence.
    #[serde(default)]
    pub callsign: Option<String>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct Store {
    nicks: HashMap<String, NickAccount>,
    /// Hosts refused at connect. Survives restart with the nick file.
    #[serde(default)]
    ip_bans: Vec<String>,
}

pub struct Accounts {
    path: PathBuf,
    store: Store,
    persist_lock: Arc<Mutex<()>>,
    persist_gen: Arc<AtomicU64>,
    async_persist: AtomicBool,
    persist_fail: Arc<AtomicU64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AccountError {
    TooShort,
    TooLong,
    Hash,
    Io,
    Taken,
    BadPassword,
    NotRegistered,
    CallsignTaken,
}

impl Accounts {
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            store: Store::default(),
            persist_lock: Arc::new(Mutex::new(())),
            persist_gen: Arc::new(AtomicU64::new(0)),
            async_persist: AtomicBool::new(false),
            persist_fail: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Write the nick file from a background thread so fsync does not stall
    /// the server actor. In-memory updates still happen first; a failed write
    /// is counted so the operator can be told. Tests leave this off so they
    /// can read the file immediately.
    pub fn enable_async_persist(&self) {
        self.async_persist.store(true, Ordering::Release);
    }

    /// How many background nick-file writes have failed since start.
    pub fn persist_failures(&self) -> u64 {
        self.persist_fail.load(Ordering::Relaxed)
    }

    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::empty(path.to_path_buf()));
        }
        let text = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!(
                "nick accounts file {} exists but cannot be read ({e}); \
                 refusing to start with an empty store that would overwrite it",
                path.display()
            )
        })?;
        let store: Store = serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "nick accounts file {} is unreadable JSON ({e}); \
                 refusing to start with an empty store that would overwrite it",
                path.display()
            )
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            store,
            persist_lock: Arc::new(Mutex::new(())),
            persist_gen: Arc::new(AtomicU64::new(0)),
            async_persist: AtomicBool::new(false),
            persist_fail: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn is_registered(&self, nick: &str) -> bool {
        self.store.nicks.contains_key(&lower(nick))
    }

    pub fn get(&self, nick: &str) -> Option<&NickAccount> {
        self.store.nicks.get(&lower(nick))
    }

    /// Insert a nick whose password was already hashed off the event loop.
    pub fn insert_hashed(&mut self, nick: &str, password_hash: String) -> Result<(), AccountError> {
        let key = lower(nick);
        let created_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let nick = nick.to_string();
        self.mutate(|store| {
            if store.nicks.contains_key(&key) {
                return Err(AccountError::Taken);
            }
            store.nicks.insert(
                key.clone(),
                NickAccount {
                    nick,
                    password_hash,
                    created_unix,
                    rf_tx: false,
                    callsign: None,
                },
            );
            Ok(())
        })
    }

    pub fn set_password_hash(
        &mut self,
        nick: &str,
        password_hash: String,
    ) -> Result<(), AccountError> {
        let key = lower(nick);
        self.mutate(|store| {
            let Some(acc) = store.nicks.get_mut(&key) else {
                return Err(AccountError::NotRegistered);
            };
            acc.password_hash = password_hash;
            Ok(())
        })
    }

    pub fn drop_nick(&mut self, nick: &str) -> Result<(), AccountError> {
        let key = lower(nick);
        self.mutate(|store| {
            if store.nicks.remove(&key).is_none() {
                return Err(AccountError::NotRegistered);
            }
            Ok(())
        })
    }

    pub fn set_rf_tx(&mut self, nick: &str, rf_tx: bool) -> Result<(), AccountError> {
        let key = lower(nick);
        self.mutate(|store| {
            let Some(acc) = store.nicks.get_mut(&key) else {
                return Err(AccountError::NotRegistered);
            };
            acc.rf_tx = rf_tx;
            Ok(())
        })
    }

    pub fn set_callsign(&mut self, nick: &str, callsign: &str) -> Result<(), AccountError> {
        let key = lower(nick);
        let callsign = callsign.to_string();
        self.mutate(|store| {
            if !store.nicks.contains_key(&key) {
                return Err(AccountError::NotRegistered);
            }
            let want = lower(&callsign);
            if let Some(owner) = store.nicks.values().find_map(|a| {
                a.callsign
                    .as_ref()
                    .filter(|c| lower(c) == want)
                    .map(|_| lower(&a.nick))
            }) {
                if owner != key {
                    return Err(AccountError::CallsignTaken);
                }
            }
            let Some(acc) = store.nicks.get_mut(&key) else {
                return Err(AccountError::NotRegistered);
            };
            acc.callsign = Some(callsign);
            Ok(())
        })
    }

    /// Which registered nick owns this callsign, if any.
    pub fn owner_of_callsign(&self, callsign: &str) -> Option<String> {
        let want = lower(callsign);
        self.store.nicks.values().find_map(|a| {
            a.callsign
                .as_ref()
                .filter(|c| lower(c) == want)
                .map(|_| a.nick.clone())
        })
    }

    pub fn clear_callsign(&mut self, callsign: &str) -> Result<String, AccountError> {
        let want = lower(callsign);
        let mut released = String::new();
        self.mutate(|store| {
            let key = store.nicks.iter().find_map(|(k, a)| {
                a.callsign
                    .as_ref()
                    .filter(|c| lower(c) == want)
                    .map(|_| k.clone())
            });
            let Some(key) = key else {
                return Err(AccountError::NotRegistered);
            };
            released = store.nicks[&key].nick.clone();
            if let Some(acc) = store.nicks.get_mut(&key) {
                acc.callsign = None;
            }
            Ok(())
        })?;
        Ok(released)
    }

    pub fn list(&self) -> Vec<&NickAccount> {
        let mut v: Vec<_> = self.store.nicks.values().collect();
        v.sort_by(|a, b| lower(&a.nick).cmp(&lower(&b.nick)));
        v
    }

    pub fn ban_ip(&mut self, host: &str) -> Result<bool, AccountError> {
        let key = host_ban_key(host);
        if key.is_empty() {
            return Ok(false);
        }
        if self.is_ip_banned(host) {
            return Ok(false);
        }
        self.mutate(|store| {
            store.ip_bans.push(key.clone());
            Ok(true)
        })
    }

    pub fn unban_ip(&mut self, host: &str) -> Result<bool, AccountError> {
        if !self.is_ip_banned(host) {
            return Ok(false);
        }
        let key = host_ban_key(host);
        self.mutate(|store| {
            store.ip_bans.retain(|h| host_ban_key(h) != key);
            Ok(true)
        })
    }

    pub fn is_ip_banned(&self, host: &str) -> bool {
        let key = host_ban_key(host);
        self.store.ip_bans.iter().any(|h| host_ban_key(h) == key)
    }

    pub fn ip_bans(&self) -> &[String] {
        &self.store.ip_bans
    }

    pub fn grants_rf_tx(&self, nick: &str) -> bool {
        self.store
            .nicks
            .get(&lower(nick))
            .map(|a| a.rf_tx)
            .unwrap_or(false)
    }

    pub fn hash_for(&self, nick: &str) -> Option<String> {
        self.store
            .nicks
            .get(&lower(nick))
            .map(|a| a.password_hash.clone())
    }

    /// Apply `f` to a copy of the store, persist that copy, then commit it.
    /// A failed write leaves the in-memory store unchanged.
    fn mutate<R>(
        &mut self,
        f: impl FnOnce(&mut Store) -> Result<R, AccountError>,
    ) -> Result<R, AccountError> {
        let mut next = self.store.clone();
        let result = f(&mut next)?;
        self.commit(next)?;
        Ok(result)
    }

    fn commit(&mut self, next: Store) -> Result<(), AccountError> {
        let text = serde_json::to_string_pretty(&next).map_err(|_| AccountError::Io)?;
        let path = self.path.clone();
        if self.async_persist.load(Ordering::Acquire) {
            // The process must see the change immediately (IDENTIFY after
            // REGISTER). Durability is best-effort; failures are counted.
            self.store = next;
            let gen = self.persist_gen.fetch_add(1, Ordering::AcqRel) + 1;
            let persist_gen = self.persist_gen.clone();
            let persist_lock = self.persist_lock.clone();
            let persist_fail = self.persist_fail.clone();
            std::thread::spawn(move || {
                let _g = persist_lock.lock().unwrap_or_else(|e| e.into_inner());
                if persist_gen.load(Ordering::Acquire) != gen {
                    return;
                }
                if let Err(e) = write_atomic(&path, &text) {
                    tracing::error!(path = %path.display(), "nick database write failed: {e:?}");
                    persist_fail.fetch_add(1, Ordering::Relaxed);
                }
            });
            return Ok(());
        }
        let _g = self.persist_lock.lock().unwrap_or_else(|e| e.into_inner());
        write_atomic(&path, &text)?;
        self.store = next;
        Ok(())
    }
}

/// Hold a global Argon2 slot, then run `work` on the blocking pool.
///
/// `Oper` waits on its own reserved slot first and only falls back to the
/// shared pool if another OPER is already using it, so an operator is never
/// queued behind unauthenticated IDENTIFY traffic.
pub async fn run_password_work<F, T>(class: WorkClass, work: F) -> Result<T, AccountError>
where
    F: FnOnce() -> Result<T, AccountError> + Send + 'static,
    T: Send + 'static,
{
    // Both semaphores are statics, so a permit from either has the same
    // type and the same job: stay alive until the hash is done.
    let _permit = match class {
        WorkClass::Oper => match ARGON2_OPER_SLOT.try_acquire() {
            Ok(permit) => permit,
            Err(_) => ARGON2_SLOTS
                .acquire()
                .await
                .map_err(|_| AccountError::Hash)?,
        },
        WorkClass::Account => ARGON2_SLOTS
            .acquire()
            .await
            .map_err(|_| AccountError::Hash)?,
    };
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or(Err(AccountError::Hash))
}

/// True if `s` is an Argon2 PHC string, not a plaintext OPER password.
pub fn is_phc_hash(s: &str) -> bool {
    PasswordHash::new(s).is_ok()
}

fn write_atomic(path: &Path, text: &str) -> Result<(), AccountError> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => {
            std::fs::create_dir_all(p).map_err(|_| AccountError::Io)?;
            p.to_path_buf()
        }
        _ => PathBuf::from("."),
    };
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "nicks.json".into())
    ));
    {
        use std::io::Write as _;
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp)
                .map_err(|_| AccountError::Io)?;
            let mut perms = f.metadata().map_err(|_| AccountError::Io)?.permissions();
            perms.set_mode(0o600);
            f.set_permissions(perms).map_err(|_| AccountError::Io)?;
            f
        };
        #[cfg(not(unix))]
        let mut f = std::fs::File::create(&temp).map_err(|_| AccountError::Io)?;
        f.write_all(text.as_bytes()).map_err(|_| AccountError::Io)?;
        f.sync_all().map_err(|_| AccountError::Io)?;
    }
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        let _ = e;
        AccountError::Io
    })?;
    // The contents were synced before the rename, but the rename itself lives
    // in the directory, and that entry is not durable until the directory is
    // synced too — so a crash here could leave the old file with the new one
    // never having replaced it. Best-effort: some filesystems refuse to sync
    // a directory handle, and failing the write over that would be worse than
    // the window it closes.
    #[cfg(unix)]
    {
        if let Ok(dir) = std::fs::File::open(&parent) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .map_err(|_| AccountError::Io)?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).map_err(|_| AccountError::Io)?;
    }
    Ok(())
}

/// Compare IRC host strings the way KLINE stores them: lowercase, no IPv6
/// brackets, IPv4-mapped IPv6 folded to IPv4 so `10.0.0.1` matches
/// `[::ffff:10.0.0.1]`.
pub fn host_ban_key(host: &str) -> String {
    let trimmed = host.trim().trim_matches(|c| c == '[' || c == ']');
    let lowered = lower(trimmed);
    if let Ok(ip) = lowered.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.to_string(),
            std::net::IpAddr::V6(v6) => v6
                .to_ipv4_mapped()
                .map(|v4| v4.to_string())
                .unwrap_or_else(|| v6.to_string()),
        };
    }
    lowered
}

pub fn hash_password(password: &str) -> Result<String, AccountError> {
    let salt = SaltString::generate(&mut OsRng);
    hasher()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| AccountError::Hash)
}

pub(crate) fn verify_password(password: &str, hash: &str) -> Result<(), AccountError> {
    let parsed = PasswordHash::new(hash).map_err(|_| AccountError::Hash)?;
    hasher()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AccountError::BadPassword)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rfircd-nicks-{n}.json"))
    }

    /// Exercise the same path the server uses: hash off the event loop,
    /// then hand the finished hash to the store.
    fn add(a: &mut Accounts, nick: &str, password: &str) -> Result<(), AccountError> {
        let hash = hash_password(password)?;
        a.insert_hashed(nick, hash)
    }

    #[test]
    fn register_verify_drop() {
        let path = tmp();
        let mut a = Accounts::empty(&path);
        add(&mut a, "Alice", "secret12").unwrap();
        assert!(a.is_registered("alice"));
        let hash = a.hash_for("ALICE").expect("case-insensitive lookup");
        assert_eq!(verify_password("secret12", &hash), Ok(()));
        assert_eq!(
            verify_password("wrongwrong", &hash),
            Err(AccountError::BadPassword)
        );
        assert_eq!(add(&mut a, "alice", "secret12"), Err(AccountError::Taken));
        a.drop_nick("alice").unwrap();
        assert!(!a.is_registered("alice"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persists_across_load() {
        let path = tmp();
        let mut a = Accounts::empty(&path);
        add(&mut a, "bob", "hunter2x").unwrap();
        a.set_rf_tx("bob", true).unwrap();
        a.set_callsign("bob", "SM0XYZ").unwrap();
        let b = Accounts::load(&path).unwrap();
        let hash = b.hash_for("bob").unwrap();
        assert_eq!(verify_password("hunter2x", &hash), Ok(()));
        assert!(b.grants_rf_tx("bob"));
        assert_eq!(
            b.get("bob").and_then(|a| a.callsign.as_deref()),
            Some("SM0XYZ")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn saving_replaces_the_file_atomically_and_leaves_no_litter() {
        let path = tmp();
        let mut a = Accounts::empty(&path);
        add(&mut a, "alice", "password1").unwrap();
        add(&mut a, "bob", "password2").unwrap();
        a.set_rf_tx("bob", true).unwrap();

        // The database reloads, and the temp file is not left behind.
        let b = Accounts::load(&path).unwrap();
        assert!(b.is_registered("alice") && b.grants_rf_tx("bob"));
        let temp = path.parent().unwrap().join(format!(
            ".{}.tmp",
            path.file_name().unwrap().to_string_lossy()
        ));
        assert!(!temp.exists(), "a temp file was left behind: {temp:?}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_rehash_of_an_old_entry_still_verifies() {
        // Argon2 parameters live in the PHC string, so raising them must not
        // lock out anybody who registered under the old cost.
        let weak = {
            let params = Params::new(4096, 2, 1, None).unwrap();
            let salt = SaltString::generate(&mut OsRng);
            Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
                .hash_password(b"legacypass", &salt)
                .unwrap()
                .to_string()
        };
        assert_eq!(verify_password("legacypass", &weak), Ok(()));
        assert_eq!(
            verify_password("wrongpass", &weak),
            Err(AccountError::BadPassword)
        );
        assert!(is_phc_hash(&weak));
        assert!(!is_phc_hash("operpass1"));
        assert!(is_phc_hash(DUMMY_OPER_PHC));
        assert_eq!(
            verify_password("wrongpass", DUMMY_OPER_PHC),
            Err(AccountError::BadPassword)
        );
    }

    #[test]
    fn a_corrupt_nick_file_is_not_silently_replaced() {
        let path = tmp();
        std::fs::write(&path, "{").unwrap();
        let err = match Accounts::load(&path) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("corrupt nick file loaded as an empty store"),
        };
        assert!(err.contains("overwrite") || err.contains("JSON"), "{err}");
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn nick_file_is_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp();
        let mut a = Accounts::empty(&path);
        add(&mut a, "alice", "password1").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "nick database was {mode:o}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_callsign_belongs_to_one_nick() {
        let path = tmp();
        let mut a = Accounts::empty(&path);
        add(&mut a, "alice", "password1").unwrap();
        add(&mut a, "bob", "password2").unwrap();
        a.set_callsign("alice", "SM0XYZ").unwrap();
        assert_eq!(a.owner_of_callsign("sm0xyz").as_deref(), Some("alice"));
        assert_eq!(
            a.set_callsign("bob", "SM0XYZ"),
            Err(AccountError::CallsignTaken)
        );
        a.set_callsign("alice", "SM0XYZ").unwrap();
        let owner = a.clear_callsign("SM0XYZ").unwrap();
        assert_eq!(owner, "alice");
        a.set_callsign("bob", "SM0XYZ").unwrap();
        assert_eq!(a.owner_of_callsign("SM0XYZ").as_deref(), Some("bob"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ip_bans_persist() {
        let path = tmp();
        let mut a = Accounts::empty(&path);
        assert!(a.ban_ip("203.0.113.9").unwrap());
        assert!(!a.ban_ip("203.0.113.9").unwrap());
        assert!(a.is_ip_banned("[203.0.113.9]"));
        let b = Accounts::load(&path).unwrap();
        assert!(b.is_ip_banned("203.0.113.9"));
        assert_eq!(b.ip_bans(), &["203.0.113.9".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_failed_save_does_not_keep_the_in_memory_change() {
        // Parent path is a file, so create_dir_all / rename cannot succeed.
        let blocker = tmp();
        std::fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join("nicks.json");
        let mut a = Accounts::empty(&path);
        assert_eq!(add(&mut a, "alice", "password1"), Err(AccountError::Io));
        assert!(
            !a.is_registered("alice"),
            "a failed write must not occupy the nick in memory"
        );
        let _ = std::fs::remove_file(blocker);
    }

    #[test]
    fn ipv4_mapped_bans_match_plain_v4() {
        assert_eq!(host_ban_key("[::ffff:10.0.0.1]"), "10.0.0.1");
        assert_eq!(host_ban_key("::ffff:10.0.0.1"), "10.0.0.1");
        assert_eq!(host_ban_key("10.0.0.1"), "10.0.0.1");
        let path = tmp();
        let mut a = Accounts::empty(&path);
        assert!(a.ban_ip("[::ffff:203.0.113.9]").unwrap());
        assert!(a.is_ip_banned("203.0.113.9"));
        assert!(a.unban_ip("203.0.113.9").unwrap());
        assert!(!a.is_ip_banned("[::ffff:203.0.113.9]"));
        let _ = std::fs::remove_file(path);
    }
}
