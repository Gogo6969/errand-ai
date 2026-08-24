//! Installing the background runner as a LaunchAgent.
//!
//! The daemon is what makes a scheduled task fire with the window closed, so
//! this is the difference between the product and a to-do list. Three traps are
//! handled here explicitly because each one fails silently:
//!
//! 1. **App translocation.** Running a quarantined download from Downloads or a
//!    DMG puts the executable at a randomized read-only path. Baked into a
//!    plist, that path is invalid after the next login and the runner never
//!    starts again.
//! 2. **launchd does not expand `~`.** Every path written here is absolute.
//! 3. **`KeepAlive: {SuccessfulExit: false}`** means a clean `exit(0)` stays
//!    down. That is what makes the update handover work: quiesce exits zero and
//!    launchd leaves it alone until the installer kickstarts the new binary.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// What a service manager must do. Only launchd exists today; Windows Task
/// Scheduler and systemd user units slot in behind this later.
pub trait ServiceManager {
    fn install(&self, exe: &Path) -> Result<PathBuf>;
    fn uninstall(&self) -> Result<()>;
    fn kickstart(&self) -> Result<()>;
    fn is_loaded(&self) -> bool;
}

pub struct Launchd;

impl Launchd {
    fn uid() -> u32 {
        // Safe: getuid cannot fail and has no side effects.
        unsafe { libc_getuid() }
    }
}

// Avoid pulling in the whole libc crate for one call.
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// Render the LaunchAgent plist. Separated from writing so it can be tested
/// without touching the user's LaunchAgents directory.
pub fn render_plist(exe: &Path, logs_dir: &Path) -> String {
    let label = crate::LAUNCHD_LABEL;
    let exe = exe.to_string_lossy();
    let out = logs_dir.join("runner.out.log");
    let err = logs_dir.join("runner.err.log");
    let home = dirs::home_dir().unwrap_or_default();
    // The daemon shells out to the claude CLI, so it needs a PATH that contains
    // the usual install locations. launchd gives a bare PATH otherwise.
    let path = format!(
        "{}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        home.to_string_lossy()
    );

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--launchd</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>
</dict>
</plist>
"#,
        out = out.to_string_lossy(),
        err = err.to_string_lossy(),
    )
}

impl ServiceManager for Launchd {
    fn install(&self, exe: &Path) -> Result<PathBuf> {
        if crate::paths::is_translocated(exe) {
            bail!(
                "Errand-AI is running from a temporary location ({}). \
                 Move it to your Applications folder first, otherwise the background \
                 runner will stop working after the next login.",
                exe.display()
            );
        }
        if !exe.exists() {
            bail!("runner binary not found at {}", exe.display());
        }

        let plist_path = crate::paths::launch_agent_plist()?;
        if let Some(parent) = plist_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let logs = crate::paths::logs_dir()?;
        std::fs::create_dir_all(&logs)?;

        std::fs::write(&plist_path, render_plist(exe, &logs))
            .with_context(|| format!("writing {}", plist_path.display()))?;

        let domain = format!("gui/{}", Self::uid());
        // bootout first so re-installing over an older definition is clean.
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{}", crate::LAUNCHD_LABEL)])
            .output();

        let out = std::process::Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(&plist_path)
            .output()
            .context("running launchctl bootstrap")?;
        if !out.status.success() {
            bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(plist_path)
    }

    fn uninstall(&self) -> Result<()> {
        let domain = format!("gui/{}/{}", Self::uid(), crate::LAUNCHD_LABEL);
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &domain])
            .output();
        let plist_path = crate::paths::launch_agent_plist()?;
        if plist_path.exists() {
            std::fs::remove_file(&plist_path)?;
        }
        Ok(())
    }

    /// `kickstart -k` kills any survivor rather than racing it for the lock
    /// file, which is what makes the post-update restart deterministic.
    fn kickstart(&self) -> Result<()> {
        let target = format!("gui/{}/{}", Self::uid(), crate::LAUNCHD_LABEL);
        let out = std::process::Command::new("launchctl")
            .args(["kickstart", "-k", &target])
            .output()
            .context("running launchctl kickstart")?;
        if !out.status.success() {
            bail!(
                "launchctl kickstart failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn is_loaded(&self) -> bool {
        let target = format!("gui/{}/{}", Self::uid(), crate::LAUNCHD_LABEL);
        std::process::Command::new("launchctl")
            .args(["print", &target])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_has_no_tilde_and_the_right_keepalive() {
        let plist = render_plist(
            Path::new("/Applications/Errand-AI.app/Contents/MacOS/errandd"),
            Path::new("/Users/USER/Library/Application Support/com.errandai.app/logs"),
        );
        assert!(!plist.contains('~'), "launchd does not expand tilde");
        assert!(plist.contains("<key>SuccessfulExit</key>\n        <false/>"));
        assert!(plist.contains("com.errandai.runner"));
        assert!(plist.contains("--launchd"));
        assert!(plist.contains("<string>Aqua</string>"));
    }

    #[test]
    fn refuses_to_install_from_a_translocated_path() {
        let translocated = Path::new(
            "/private/var/folders/xy/AppTranslocation/ABC-123/d/Errand-AI.app/Contents/MacOS/errandd",
        );
        assert!(crate::paths::is_translocated(translocated));
        let err = Launchd.install(translocated).unwrap_err();
        assert!(err.to_string().contains("Applications folder"));
    }

    #[test]
    fn a_mounted_dmg_counts_as_translocated() {
        assert!(crate::paths::is_translocated(Path::new(
            "/Volumes/Errand-AI/Errand-AI.app/Contents/MacOS/errandd"
        )));
    }
}
