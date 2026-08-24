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

/// The credential store for this platform.
pub fn store() -> impl CredStore {
    MacKeychain
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

    #[test]
    fn secret_never_prints_itself() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{s}"), "[redacted]");
        assert!(!format!("{s:?}").contains("hunter2"));
        assert!(format!("{s:?}").contains("7 bytes"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_roundtrip() {
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
    }
}
