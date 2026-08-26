//! Where the refresh token lives between runs: the `TokenStore` trait, the
//! two implementations behind it (the GNOME keyring, and a 0600 file), the
//! selection between them, and `TokenWrites`, which serialises every write
//! against a revocation.
//!
//! Split out of `auth.rs`, which is about the OAuth protocol. The only thing
//! the two share is the string that travels between them: nothing in here
//! knows what a refresh token *is*, and nothing in `auth` knows where one is
//! kept.
//!
//! Nothing in this module ever prints, logs or formats a token.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

/// How long any single Secret Service call may take before it is abandoned.
const KEYRING_TIMEOUT: StdDuration = StdDuration::from_secs(10);
/// The probe in `token_store` runs before the user has asked for anything, so
/// it gets far less patience than a call they explicitly triggered.
const KEYRING_PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(3);

pub trait TokenStore: Send + Sync {
    fn save(&self, token: &str) -> anyhow::Result<()>;
    fn load(&self) -> anyhow::Result<Option<String>>;
    fn clear(&self) -> anyhow::Result<()>;
    fn describe(&self) -> &'static str;
}

/// Runs a blocking Secret Service call on a thread of its own and abandons it
/// when it overruns.
///
/// This exists because a locked collection turns `get_password` into a modal
/// unlock dialog on the user's desktop, and that call does not return until the
/// dialog is answered — which may be never, if the screen is locked or the
/// session is remote. Abandoning the thread leaks it until the prompt resolves;
/// that is a far better outcome than a panel widget that never starts.
fn guarded<T: Send + 'static>(
    timeout: StdDuration,
    what: &'static str,
    op: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(op());
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => anyhow::bail!(
            "the keyring did not answer within {}s while trying to {what} — it may be locked",
            timeout.as_secs()
        ),
    }
}

pub struct KeyringStore;

impl KeyringStore {
    fn entry() -> anyhow::Result<keyring::Entry> {
        Ok(keyring::Entry::new("protector", "google-refresh-token")?)
    }

    fn load_within(timeout: StdDuration) -> anyhow::Result<Option<String>> {
        guarded(timeout, "read the token", || match Self::entry()?.get_password() {
            Ok(t) => Ok(Some(t)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        })
    }
}

impl TokenStore for KeyringStore {
    fn save(&self, token: &str) -> anyhow::Result<()> {
        let token = token.to_string();
        guarded(KEYRING_TIMEOUT, "store the token", move || {
            Self::entry()?.set_password(&token)?;
            Ok(())
        })
    }
    fn load(&self) -> anyhow::Result<Option<String>> {
        Self::load_within(KEYRING_TIMEOUT)
    }
    fn clear(&self) -> anyhow::Result<()> {
        guarded(KEYRING_TIMEOUT, "forget the token", || {
            match Self::entry()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
    }
    fn describe(&self) -> &'static str {
        "GNOME keyring"
    }
}

pub struct FileStore(pub PathBuf);

impl FileStore {
    /// Where `save` stages the token before renaming it into place. `clear`
    /// has to know this path too, which is why it is named once here rather
    /// than spelled out at both sites.
    fn staging_path(&self) -> PathBuf {
        self.0.with_extension("tmp")
    }
}

fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

impl TokenStore for FileStore {
    /// Written to a sibling temp file and renamed over the target, the same way
    /// `state::save` works. A truncate-then-write would turn a crash mid-save
    /// into a lost refresh token and a forced re-login.
    fn save(&self, token: &str) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = self.0.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.staging_path();
        // 0600 on the temp file too: the secret is in it from the first write,
        // so it must never exist as a world-readable file, not even briefly.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        let write = f.write_all(token.as_bytes()).and_then(|()| f.sync_all());
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        drop(f);
        if let Err(e) = std::fs::rename(&tmp, &self.0) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }
    fn load(&self) -> anyhow::Result<Option<String>> {
        match std::fs::read_to_string(&self.0) {
            // Trimmed: a hand-edited file gains a trailing newline, and an
            // empty file is "no token", not a token that happens to be blank.
            Ok(t) if t.trim().is_empty() => Ok(None),
            Ok(t) => Ok(Some(t.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    /// Removes the staging file as well as the token itself.
    ///
    /// `save` writes the refresh token into `token.tmp` and only then renames
    /// it over `token.json`; a crash in that window leaves a **live**
    /// credential in the staging file that nothing else ever touches. Logout
    /// promises to clear the token from every place it could be, and that is
    /// one of the places.
    ///
    /// Both removals are attempted even when the first one fails, so a
    /// permission problem on one cannot shield the other.
    fn clear(&self) -> anyhow::Result<()> {
        let token = remove_if_present(&self.0);
        let staged = remove_if_present(&self.staging_path());
        token.and(staged)
    }
    fn describe(&self) -> &'static str {
        "file (0600)"
    }
}

pub fn token_file_path() -> PathBuf {
    crate::state::state_path().with_file_name("token.json")
}

/// True when a session bus could plausibly exist. Without one there is no Secret
/// Service to talk to, so the keyring probe below would only burn a timeout.
fn session_bus_present() -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| PathBuf::from(d).join("bus").exists())
        .unwrap_or(false)
}

/// Every store a refresh token could be sitting in on this machine.
///
/// `token_store()` picks *one*, and which one it picks can differ between runs:
/// a login that fell back to the file because the keyring was locked, followed
/// by a logout with the keyring reachable, would clear the keyring and leave a
/// live credential in the file for good. So revocation and the single-copy
/// invariant are both defined over this list, never over the selected store.
///
/// The keyring is listed only when a session bus exists, because without one it
/// cannot hold anything. Constructing the entries touches no I/O.
pub fn all_token_stores() -> Vec<Box<dyn TokenStore>> {
    let mut stores: Vec<Box<dyn TokenStore>> = vec![Box::new(FileStore(token_file_path()))];
    if session_bus_present() {
        stores.push(Box::new(KeyringStore));
    }
    stores
}

/// Decided once per process. `guarded` spawns a thread per keyring call, and the
/// probe costs up to `KEYRING_PROBE_TIMEOUT` against a locked collection — the
/// sync loop must not pay either of those on every pass.
static SELECTED_STORE: std::sync::OnceLock<Box<dyn TokenStore>> = std::sync::OnceLock::new();

fn select_token_store() -> Box<dyn TokenStore> {
    if session_bus_present() && KeyringStore::load_within(KEYRING_PROBE_TIMEOUT).is_ok() {
        Box::new(KeyringStore)
    } else {
        Box::new(FileStore(token_file_path()))
    }
}

/// Prefers the keyring; falls back to a 0600 file when Secret Service is absent
/// or does not answer.
///
/// The probe is a `load()`, which is prompt-free for the case that matters — a
/// user who has never logged in has no stored item, so the Secret Service
/// answers `NoEntry` without unlocking anything. A user who *has* logged in and
/// whose collection is locked will see their desktop's unlock dialog here; the
/// watchdog in `guarded` bounds how long Protector waits for it.
///
/// Blocking on first call: reach it from `tokio::task::spawn_blocking` in async
/// code. Later calls are free.
pub fn token_store() -> &'static dyn TokenStore {
    SELECTED_STORE.get_or_init(select_token_store).as_ref()
}

/// A zero-sized handle to whatever `token_store()` selects, so the sync path can
/// move a copy into each `spawn_blocking` call it makes. A `&'static dyn` cannot
/// be moved into a `'static` closure that outlives the borrow it came from; an
/// `Arc` can, and this one costs no allocation of its own beyond the `Arc`.
///
/// Constructing it is free: the selection — and its keyring probe — still
/// happens on the first `save`/`load`/`clear`, inside whatever thread makes it.
struct SelectedStore;

impl TokenStore for SelectedStore {
    fn save(&self, token: &str) -> anyhow::Result<()> {
        token_store().save(token)
    }
    fn load(&self) -> anyhow::Result<Option<String>> {
        token_store().load()
    }
    fn clear(&self) -> anyhow::Result<()> {
        token_store().clear()
    }
    fn describe(&self) -> &'static str {
        token_store().describe()
    }
}

pub fn shared_token_store() -> std::sync::Arc<dyn TokenStore> {
    std::sync::Arc::new(SelectedStore)
}

/// Serialises every write to the token store against revocation.
///
/// Cancellation cannot do this job, and it is worth being explicit about why:
/// `tokio::task::spawn_blocking` runs a closure it has already dispatched to
/// completion no matter what becomes of its `JoinHandle`, and `KeyringStore`
/// puts a second raw `std::thread` behind that again — one whose own watchdog
/// admits it can outlive its budget while a desktop prompt is pending. So by
/// the time a sync's rotated-token write is on its way, nothing can call it
/// back, and a "Disconnect account" landing in that window would be undone by
/// a credential written after it.
///
/// The two operations take turns instead. Every write states the epoch it
/// began in; every revocation bumps the epoch under the same lock the write
/// must hold. A write whose epoch is stale is dropped rather than applied, and
/// a write that *returns* returns before the clear that follows it.
///
/// One exception, and it is a real one rather than a theoretical one:
/// `guarded` gives a keyring call `KEYRING_TIMEOUT` and then **abandons** the
/// thread running it, returning an error and releasing this mutex while the
/// underlying `set_password` is still in flight. A revocation can then take
/// the lock, bump the epoch and clear every store, and the abandoned write can
/// land afterwards — leaving a live refresh token behind a disconnect that
/// reported success. Nothing here can call that thread back; the alternative
/// is a panel widget that hangs forever on a locked keyring, so this is the
/// trade that was chosen. `protector logout` clears every store, so a later
/// one still finds such a token.
pub struct TokenWrites {
    lock: std::sync::Mutex<()>,
    epoch: std::sync::atomic::AtomicU64,
}

impl Default for TokenWrites {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenWrites {
    pub fn new() -> Self {
        Self { lock: std::sync::Mutex::new(()), epoch: std::sync::atomic::AtomicU64::new(0) }
    }

    /// The epoch a write beginning now belongs to.
    ///
    /// Deliberately lock-free: this is read from async code, where waiting on a
    /// keyring write that may hold the lock for ten seconds would park a
    /// runtime worker thread.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A poisoned lock is no reason to strand the token store: what it guards
    /// is one counter, and every path that touches it leaves it consistent.
    fn enter(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn bump(&self) {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Writes `token` unless something has superseded it since `epoch` — a
    /// disconnect, or a newer login. Returns whether it wrote.
    ///
    /// The check and the write are one critical section on purpose: a check
    /// outside the lock would reintroduce the same race in miniature.
    ///
    /// Blocking; call it from `spawn_blocking`.
    pub fn save_unless_revoked(
        &self,
        store: &dyn TokenStore,
        token: &str,
        epoch: u64,
    ) -> anyhow::Result<bool> {
        let _guard = self.enter();
        if self.epoch() != epoch {
            return Ok(false);
        }
        store.save(token)?;
        Ok(true)
    }

    /// Writes a newly issued token, invalidating every write that began before
    /// it. Blocking.
    pub fn install(&self, store: &dyn TokenStore, token: &str) -> anyhow::Result<()> {
        let _guard = self.enter();
        self.bump();
        store.save(token)
    }

    /// Clears every store on this machine and invalidates every write that
    /// began before now. Blocking. One outcome per store, in `all_token_stores`
    /// order.
    pub fn revoke_all_stores(&self) -> Vec<(&'static str, anyhow::Result<()>)> {
        let _guard = self.enter();
        self.bump();
        all_token_stores().into_iter().map(|s| (s.describe(), s.clear())).collect()
    }

    /// Invalidates every write that began before now, without clearing
    /// anything. Blocking.
    pub fn invalidate(&self) {
        let _guard = self.enter();
        self.bump();
    }
}

static TOKEN_WRITES: std::sync::OnceLock<std::sync::Arc<TokenWrites>> = std::sync::OnceLock::new();

/// The process-wide guard. Every token write the running widget performs goes
/// through this one instance; tests use instances of their own, so that no test
/// can invalidate another's write.
pub fn token_writes() -> std::sync::Arc<TokenWrites> {
    TOKEN_WRITES.get_or_init(|| std::sync::Arc::new(TokenWrites::new())).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_store_round_trips_and_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("token.json"));
        assert!(store.load().unwrap().is_none());
        store.save("refresh-me").unwrap();
        assert_eq!(store.load().unwrap().unwrap(), "refresh-me");
        let mode = std::fs::metadata(dir.path().join("token.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn a_file_store_creates_its_directory_and_overwrites_an_older_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("nested/deeper/token.json"));
        store.save("first").unwrap();
        store.save("second").unwrap();
        assert_eq!(store.load().unwrap().unwrap(), "second");
        let mode = std::fs::metadata(&store.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "an overwrite must not widen the mode");
        // No temporary leftovers holding a copy of the token beside it.
        let strays: Vec<_> = std::fs::read_dir(store.0.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n != "token.json")
            .collect();
        assert!(strays.is_empty(), "left behind: {strays:?}");
    }

    #[test]
    fn an_empty_token_file_reads_as_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("token.json"));
        std::fs::write(&store.0, "  \n").unwrap();
        assert!(store.load().unwrap().is_none());
    }

    /// A crash between `sync_all` and `rename` in `save` leaves the refresh
    /// token in the staging file. `clear` — the one thing standing between a
    /// disconnect and a live credential on disk — has to take that with it.
    #[test]
    fn clearing_a_file_store_removes_a_token_left_in_the_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("token.json"));
        let staged = store.staging_path();
        // Exactly the state an interrupted `save` leaves behind.
        std::fs::write(&staged, "live-refresh-token").unwrap();
        store.save("another-live-token").unwrap();

        store.clear().unwrap();

        assert!(store.load().unwrap().is_none());
        assert!(
            !staged.exists(),
            "a live refresh token survived logout in {}",
            staged.display()
        );
    }

    #[test]
    fn clearing_a_store_that_holds_nothing_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        FileStore(dir.path().join("absent.json")).clear().unwrap();
    }

    #[test]
    fn a_file_store_says_how_it_stores_the_token() {
        assert!(FileStore(PathBuf::from("/tmp/x")).describe().contains("0600"));
    }

    /// Constructing the stores performs no I/O, so this never reaches the real
    /// Secret Service; it pins the list `logout` revokes over and `login`
    /// reconciles against.
    #[test]
    fn the_file_store_is_always_one_of_the_stores_logout_has_to_clear() {
        let described: Vec<_> = all_token_stores().iter().map(|s| s.describe()).collect();
        assert!(
            described.contains(&FileStore(token_file_path()).describe()),
            "a token stranded in the file store could never be revoked: {described:?}"
        );
        // `login` tells the selected store apart from the rest by `describe()`,
        // and `logout` prints one line per entry, so duplicates would silently
        // break both.
        let mut unique = described.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), described.len(), "duplicate stores: {described:?}");
    }

    // ---- Serialising token writes against revocation ------------------------

    /// Every test here uses its own `TokenWrites`, never `token_writes()`: a
    /// shared epoch would let one test invalidate another's write.
    #[test]
    fn a_write_that_began_before_a_revocation_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("token.json"));
        let writes = TokenWrites::new();

        let epoch = writes.epoch();
        // "Disconnect account", while the write above is still in flight.
        writes.invalidate();
        assert!(!writes.save_unless_revoked(&store, "1//rotated", epoch).unwrap());
        assert_eq!(store.load().unwrap(), None, "a revoked account kept a live token");

        // A write that began after it is applied as normal.
        let epoch = writes.epoch();
        assert!(writes.save_unless_revoked(&store, "1//fresh", epoch).unwrap());
        assert_eq!(store.load().unwrap().as_deref(), Some("1//fresh"));
    }

    #[test]
    fn installing_a_newer_token_invalidates_a_write_that_began_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore(dir.path().join("token.json"));
        let writes = TokenWrites::new();

        // A sync begins an attempt, then the user reconnects the account.
        let epoch = writes.epoch();
        writes.install(&store, "1//from-the-new-login").unwrap();
        // The older attempt's rotation must not overwrite the newer credential.
        assert!(!writes.save_unless_revoked(&store, "1//from-the-old-account", epoch).unwrap());
        assert_eq!(store.load().unwrap().as_deref(), Some("1//from-the-new-login"));
    }

    #[test]
    fn a_revocation_waits_for_a_write_that_is_already_under_way() {
        // The ordering that matters: the clear cannot begin while a write holds
        // the lock, so it can never be overtaken by the write it was meant to
        // undo. Proven here with the lock alone, no store involved.
        use std::sync::Arc;
        let writes = Arc::new(TokenWrites::new());
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));

        let started = writes.clone();
        let started_order = order.clone();
        let holder = std::thread::spawn(move || {
            let guard = started.enter();
            started_order.lock().unwrap().push("write begins");
            std::thread::sleep(std::time::Duration::from_millis(200));
            started_order.lock().unwrap().push("write ends");
            drop(guard);
        });
        // Long enough that the revocation below is genuinely queued behind it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        writes.invalidate();
        order.lock().unwrap().push("revocation");
        holder.join().unwrap();

        assert_eq!(*order.lock().unwrap(), vec!["write begins", "write ends", "revocation"]);
    }

    /// The keyring itself is deliberately never touched by the test suite: a
    /// locked Secret Service raises a modal unlock dialog on the developer's
    /// desktop. What *is* tested is the watchdog that keeps such a dialog from
    /// wedging Protector forever.
    #[test]
    fn a_keyring_call_that_never_answers_is_abandoned_rather_than_waited_on() {
        let started = std::time::Instant::now();
        let err = guarded(std::time::Duration::from_millis(50), "read the token", || {
            std::thread::sleep(std::time::Duration::from_secs(30));
            Ok(())
        })
        .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(format!("{err:#}").contains("read the token"), "error was: {err:#}");
    }

    #[test]
    fn a_keyring_call_that_answers_in_time_returns_its_value() {
        let got = guarded(std::time::Duration::from_secs(5), "read the token", || {
            Ok(Some("value".to_string()))
        })
        .unwrap();
        assert_eq!(got.as_deref(), Some("value"));
    }
}
