//! Every path the app uses, resolved in one place.
//!
//! Data root is `~/Library/Application Support/com.errandai.app/`. launchd does
//! not expand `~`, so anything written into a plist must come from here already
//! absolute.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// `~/Library/Application Support/com.errandai.app/`, honouring an override for
/// tests and dev runs.
pub fn data_root() -> Result<PathBuf> {
    if let Ok(over) = std::env::var("ERRAND_DATA_DIR") {
        return Ok(PathBuf::from(over));
    }
    let base = dirs::data_dir().context("no Application Support directory for this user")?;
    Ok(base.join(crate::APP_ID))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_root()?.join("errand.db"))
}

pub fn logs_dir() -> Result<PathBuf> {
    Ok(data_root()?.join("logs"))
}

pub fn runs_dir() -> Result<PathBuf> {
    Ok(data_root()?.join("runs"))
}

pub fn playbooks_dir() -> Result<PathBuf> {
    Ok(data_root()?.join("playbooks"))
}

pub fn profiles_dir() -> Result<PathBuf> {
    Ok(data_root()?.join("profiles"))
}

pub fn backups_dir() -> Result<PathBuf> {
    Ok(data_root()?.join("backups"))
}

/// Lock file guaranteeing a single daemon per user session.
pub fn runner_lock() -> Result<PathBuf> {
    Ok(data_root()?.join("runner.lock"))
}

/// Artifact directory for one run.
pub fn run_dir(run_id: &str) -> Result<PathBuf> {
    Ok(runs_dir()?.join(run_id))
}

/// `~/Library/LaunchAgents/com.errandai.runner.plist`
pub fn launch_agent_plist() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", crate::LAUNCHD_LABEL)))
}

/// Create every directory the daemon expects. Called once at boot.
pub fn ensure_dirs() -> Result<()> {
    for dir in [
        data_root()?,
        logs_dir()?,
        runs_dir()?,
        playbooks_dir()?,
        profiles_dir()?,
        backups_dir()?,
    ] {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // The data root holds credentials metadata and run artifacts. Keep it to
    // the owning user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = data_root()?;
        let mut perms = std::fs::metadata(&root)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&root, perms)?;
    }
    Ok(())
}

/// Detect Gatekeeper app translocation.
///
/// Running a quarantined download from Downloads or a mounted DMG puts the app
/// at a randomized read-only path. Baking that path into a LaunchAgent plist
/// means the runner never starts again after the next login, silently. So the
/// supervisor refuses to install until the app is in a real location.
pub fn is_translocated(exe: &std::path::Path) -> bool {
    let s = exe.to_string_lossy();
    s.contains("/AppTranslocation/") || s.starts_with("/Volumes/")
}
