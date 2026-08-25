//! Secrets, and the only code allowed to touch them.
//!
//! Items are `kSecClassGenericPassword` in the macOS **login keychain**. That
//! choice is deliberate: the login keychain is where per-item ACLs live, while
//! the data-protection keychain controls access by access group and has no
//! per-item trusted-application list at all. Generic passwords here do not sync
//! to iCloud unless explicitly marked synchronizable, which this code never
//! does.
//!
//! A secret's total exposure is: the keychain, one Rust stack frame, one pipe
//! write to the browser sidecar, and Chromium's input pipeline. It never enters
//! an AI prompt, the database, the run journal, the logs, or any API response.

use anyhow::{anyhow, Result};

/// A secret that scrubs itself when dropped and refuses to print itself.
///
/// `Debug` deliberately shows a placeholder: the single most common way secrets
/// end up in logs is a struct deriving Debug somewhere up the call stack.
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Self {
        Self(s)
    }

    /// Borrow the plaintext. Every call site should be short and end in a write
    /// to something outside this process.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret([redacted; {} bytes])", self.0.len())
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[redacted]")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Overwrite in place before the allocation goes back to the allocator.
        unsafe {
            for b in self.0.as_bytes_mut() {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}

/// The keychain operations the rest of the system is allowed to perform.
pub trait CredStore: Send + Sync {
    fn put(&self, service: &str, account: &str, secret: &Secret) -> Result<()>;
    fn get(&self, service: &str, account: &str) -> Result<Secret>;
    fn delete(&self, service: &str, account: &str) -> Result<()>;
    fn exists(&self, service: &str, account: &str) -> bool {
        self.get(service, account).is_ok()
    }
}

// ------------------------------------------------------------------- macOS --

#[cfg(target_os = "macos")]
pub struct MacKeychain;

#[cfg(target_os = "macos")]
impl CredStore for MacKeychain {
    fn put(&self, service: &str, account: &str, secret: &Secret) -> Result<()> {
        use security_framework::passwords::{delete_generic_password, set_generic_password};
        // set_generic_password updates in place on some macOS versions and
        // errors on others, so remove any existing item first.
        let _ = delete_generic_password(service, account);
        set_generic_password(service, account, secret.expose().as_bytes())
            .map_err(|e| anyhow!("keychain write failed for {service}/{account}: {e}"))
    }

    fn get(&self, service: &str, account: &str) -> Result<Secret> {
        use security_framework::passwords::get_generic_password;
        let bytes = get_generic_password(service, account)
            .map_err(|e| anyhow!("keychain read failed for {service}/{account}: {e}"))?;
        Ok(Secret::new(String::from_utf8(bytes)?))
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        use security_framework::passwords::delete_generic_password;
        delete_generic_password(service, account)
            .map_err(|e| anyhow!("keychain delete failed for {service}/{account}: {e}"))
    }
}

// ------------------------------------------------- other platforms, later --

#[cfg(not(target_os = "macos"))]
pub struct MacKeychain;

#[cfg(not(target_os = "macos"))]
impl CredStore for MacKeychain {
    fn put(&self, _: &str, _: &str, _: &Secret) -> Result<()> {
        Err(anyhow!(
            "no credential store on this platform yet; Windows Credential Manager \
             and the Linux Secret Service arrive with the cross-platform milestone"
        ))
    }
    fn get(&self, _: &str, _: &str) -> Result<Secret> {
        Err(anyhow!("no credential store on this platform yet"))
    }
    fn delete(&self, _: &str, _: &str) -> Result<()> {
        Err(anyhow!("no credential store on this platform yet"))
    }
}

// ------------------------------------------- a store for throwaway builds --

/// Secrets in a file, for builds that have no business in your real keychain.
///
/// macOS ties keychain access to a program's code signature, and `cargo build`
/// produces a new identity every time it relinks. So a development build asks
/// permission, you grant it, you rebuild, and it asks again — "Always Allow"
/// cannot stick to a program that is a different program on every compile.
/// After a day of that, the habit of clicking Allow without reading is the real
/// damage, and that habit is worth more to an attacker than anything in the
/// file this replaces.
///
/// So a debug build keeps its secrets beside its own database instead. They are
/// development secrets belonging to a development database, and treating them
/// as though they were your bank login trains exactly the wrong reflex.
pub struct FileStore {
    path: std::path::PathBuf,
}

impl FileStore {
    pub fn at(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    fn read(&self) -> serde_json::Map<String, serde_json::Value> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn write(&self, map: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
        // Each step names itself. "No such file or directory" on its own leaves
        // whoever reads it guessing which of four files was missing.
        let dir = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("{} has nowhere to live", self.path.display()))?;
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow!("could not make {}: {e}", dir.display()))?;

        // Written beside the real file and renamed over it, so an interrupted
        // write cannot leave a half-file that loses every other secret in it.
        let tmp = self.path.with_extension("writing");
        let body = serde_json::to_string_pretty(map)?;
        std::fs::write(&tmp, body)
            .map_err(|e| anyhow!("could not write {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| anyhow!("could not lock down {}: {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| anyhow!("could not move {} into place: {e}", self.path.display()))?;
        Ok(())
    }

    fn key(service: &str, account: &str) -> String {
        format!("{service}\u{1f}{account}")
    }
}

impl CredStore for FileStore {
    fn put(&self, service: &str, account: &str, secret: &Secret) -> Result<()> {
        let mut map = self.read();
        map.insert(
            Self::key(service, account),
            serde_json::Value::String(secret.expose().to_string()),
        );
        self.write(&map)
    }

    fn get(&self, service: &str, account: &str) -> Result<Secret> {
        self.read()
            .get(&Self::key(service, account))
            .and_then(|v| v.as_str())
            .map(|v| Secret::new(v.to_string()))
            .ok_or_else(|| anyhow!("nothing saved for {service}/{account}"))
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let mut map = self.read();
        if map.remove(&Self::key(service, account)).is_none() {
            return Err(anyhow!("nothing saved for {service}/{account}"));
        }
        self.write(&map)
    }
}

/// Which store this build uses, and why.
///
/// A release build always uses the keychain. A debug build uses a file, because
/// it would otherwise ask permission on every single compile — see `FileStore`.
/// `ERRAND_KEYCHAIN=on` forces the real thing for anyone deliberately testing
/// that path; `ERRAND_KEYCHAIN=off` forces the file.
pub fn using_keychain() -> bool {
    match std::env::var("ERRAND_KEYCHAIN").ok().as_deref() {
        Some("on") => true,
        Some("off") => false,
        _ => !crate::is_dev_build(),
    }
}

/// Said out loud by `doctor` and at boot, so nobody is ever unsure which one is
/// holding their logins.
pub fn store_description() -> String {
    if using_keychain() {
        "your macOS keychain".to_string()
    } else {
        match dev_secrets_path() {
            Ok(p) => format!(
                "a file, because this is a development build: {} (NOT protected by macOS)",
                p.display()
            ),
            Err(_) => "a file, because this is a development build".to_string(),
        }
    }
}

fn dev_secrets_path() -> Result<std::path::PathBuf> {
    Ok(crate::paths::data_root()?.join("dev-secrets.json"))
}

/// The credential store for this build.
pub fn store() -> Box<dyn CredStore> {
    if using_keychain() {
        Box::new(MacKeychain)
    } else {
        match dev_secrets_path() {
            Ok(p) => Box::new(FileStore::at(p)),
            // Nowhere to put a file is not a reason to silently fall back to
            // the keychain: that is the prompt loop this exists to end.
            Err(_) => Box::new(FileStore::at(std::path::PathBuf::from(
                "errand-dev-secrets.json",
            ))),
        }
    }
}

// --------------------------------------------------------- internal secrets --

/// Account name for the primary API token inside the internal keychain service.
pub const ACCOUNT_API_TOKEN: &str = "api-token-primary";

/// Persist an app-owned secret (API token, bot token) rather than a site login.
pub fn put_internal(account: &str, secret: &Secret) -> Result<()> {
    store().put(&crate::keychain_service_internal(), account, secret)
}

pub fn get_internal(account: &str) -> Result<Secret> {
    store().get(&crate::keychain_service_internal(), account)
}

pub fn delete_internal(account: &str) -> Result<()> {
    store().delete(&crate::keychain_service_internal(), account)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own. Tests share a process, so a name built
    /// from the pid is the same name for all of them, and one tidying up at the
    /// end pulls the floor out from under another mid-write.
    fn scratch(who: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("errand-{who}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn secret_never_prints_itself() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{s}"), "[redacted]");
        assert!(!format!("{s:?}").contains("hunter2"));
        assert!(format!("{s:?}").contains("7 bytes"));
    }

    #[test]
    fn a_secret_survives_being_put_away_and_fetched_back() {
        // The same round trip as the keychain one, on the store a development
        // build really uses, and it raises no dialog so it runs every time.
        let dir = scratch("roundtrip");
        let store = FileStore::at(dir.join("secrets.json"));

        assert!(store.get("svc", "acct").is_err(), "nothing is there yet");
        store
            .put("svc", "acct", &Secret::new("correct horse".into()))
            .expect("save it");
        assert_eq!(
            store.get("svc", "acct").expect("read it back").expose(),
            "correct horse"
        );

        // Two services with the same account name must not read each other.
        store
            .put("other", "acct", &Secret::new("different".into()))
            .unwrap();
        assert_eq!(store.get("svc", "acct").unwrap().expose(), "correct horse");

        store.delete("svc", "acct").expect("forget it");
        assert!(store.get("svc", "acct").is_err(), "it should be gone");
        assert!(
            store.delete("svc", "acct").is_err(),
            "forgetting it twice is an error, not a quiet success"
        );
        assert_eq!(store.get("other", "acct").unwrap().expose(), "different");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_half_written_file_cannot_lose_the_secrets_that_were_already_in_it() {
        // The reason for writing beside the real file and renaming over it.
        let dir = scratch("atomic");
        let path = dir.join("secrets.json");
        let store = FileStore::at(path.clone());
        store.put("a", "one", &Secret::new("first".into())).unwrap();
        store
            .put("b", "two", &Secret::new("second".into()))
            .unwrap();

        // Whatever is on disk must be a whole file at every moment, never a
        // partial one, so re-reading it finds both.
        let fresh = FileStore::at(path);
        assert_eq!(fresh.get("a", "one").unwrap().expose(), "first");
        assert_eq!(fresh.get("b", "two").unwrap().expose(), "second");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_development_build_does_not_reach_for_the_real_keychain() {
        // The whole point. macOS ties keychain access to a code signature and
        // every compile produces a new one, so a debug build would ask
        // permission forever — and teach the habit of clicking Allow unread,
        // which is worth more to an attacker than anything it was guarding.
        assert!(
            !using_keychain(),
            "these tests are a debug build, so they must not touch the keychain"
        );
        assert!(store_description().contains("development build"));
    }

    #[test]
    fn asking_for_the_keychain_is_still_possible_for_anyone_testing_that_path() {
        // Forced on and forced off both have to work, or the escape hatch is
        // decoration and the only way to test the real store is a release build.
        temp_env("ERRAND_KEYCHAIN", Some("on"), || assert!(using_keychain()));
        temp_env(
            "ERRAND_KEYCHAIN",
            Some("off"),
            || assert!(!using_keychain()),
        );
    }

    /// Set an environment variable for the duration of one closure.
    ///
    /// Tests share a process, so this is only safe because the two that use it
    /// are the only ones that read this variable.
    fn temp_env(key: &str, value: Option<&str>, f: impl FnOnce()) {
        let old = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match old {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn a_secrets_file_is_not_readable_by_anyone_else() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = scratch("perm");
            let path = dir.join("secrets.json");
            FileStore::at(path.clone())
                .put("svc", "acct", &Secret::new("x".into()))
                .unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "a secrets file must be readable only by its owner"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // Ignored on purpose. It talks to the real macOS keychain, which means an
    // OS permission dialog, and an ordinary `cargo test` must never raise one:
    // a test suite that interrupts you is a test suite you stop running.
    // Run it deliberately with `ERRAND_KEYCHAIN=on cargo test -- --ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "talks to the real keychain and raises an OS permission prompt"]
    fn keychain_roundtrip() {
        temp_env("ERRAND_KEYCHAIN", Some("on"), || {
            let store = store();
            let service = format!("{}.selftest", crate::keychain_service_internal());
            let account = "roundtrip";
            let _ = store.delete(&service, account);

            store
                .put(&service, account, &Secret::new("correct horse".into()))
                .expect("write to keychain");
            let got = store.get(&service, account).expect("read back");
            assert_eq!(got.expose(), "correct horse");

            store.delete(&service, account).expect("delete");
            assert!(store.get(&service, account).is_err(), "item should be gone");
        });
    }
}
