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

/// The variables that point Errand at a different Node, a different sidecar
/// script or a different browser. The daemon reads all three at launch, so
/// without them here the documented workaround works when you run the runner
/// from a terminal and does nothing at all for the process that actually runs
/// the tasks — which is the only process that matters at 08:00.
const PASSTHROUGH_ENV: [&str; 3] = ["ERRAND_NODE", "ERRAND_SIDECAR", "ERRAND_BROWSER"];

/// Render the LaunchAgent plist. Separated from writing so it can be tested
/// without touching the user's LaunchAgents directory.
pub fn render_plist(exe: &Path, logs_dir: &Path) -> String {
    // Taken from the environment doing the installing, because that is where
    // someone who has just been told to set ERRAND_NODE will have set it.
    let extra: Vec<(String, String)> = PASSTHROUGH_ENV
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
        .filter(|(_, v)| !v.trim().is_empty())
        .collect();
    render_plist_with(exe, logs_dir, &extra)
}

/// A path or a browser name can contain `&`, and an unescaped one makes the
/// whole plist unparseable — which launchd reports as the agent simply not
/// being there.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render_plist_with(exe: &Path, logs_dir: &Path, extra_env: &[(String, String)]) -> String {
    let label = crate::LAUNCHD_LABEL;
    let exe = xml_escape(&exe.to_string_lossy());
    let out = xml_escape(&logs_dir.join("runner.out.log").to_string_lossy());
    let err = xml_escape(&logs_dir.join("runner.err.log").to_string_lossy());
    let home = dirs::home_dir().unwrap_or_default();
    // The daemon shells out to the claude CLI, so it needs a PATH that contains
    // the usual install locations. launchd gives a bare PATH otherwise.
    //
    // Volta is in here because it is the one version manager with a fixed shim
    // directory; nvm and fnm keep node under a per-version directory with no
    // stable name, so they cannot be named in a static PATH and are found by
    // the runner's own search instead.
    let path = xml_escape(&format!(
        "{home}/.local/bin:{home}/.volta/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        home = home.to_string_lossy()
    ));
    let extra = extra_env
        .iter()
        .map(|(k, v)| {
            format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            )
        })
        .collect::<String>();

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
{extra}    </dict>
</dict>
</plist>
"#
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
    fn the_daemon_inherits_the_escape_hatch_variables_that_were_set_when_it_was_installed() {
        // Someone who has been told "set ERRAND_NODE to your node" has fixed
        // nothing if the fix stops at their shell.
        let plist = render_plist_with(
            Path::new("/Applications/Errand-AI.app/Contents/MacOS/errandd"),
            Path::new("/Users/USER/Library/Application Support/com.errandai.app/logs"),
            &[
                (
                    "ERRAND_NODE".to_string(),
                    "/Users/USER/.nvm/versions/node/v22.11.0/bin/node".to_string(),
                ),
                (
                    "ERRAND_BROWSER".to_string(),
                    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser".to_string(),
                ),
            ],
        );
        assert!(plist.contains("<key>ERRAND_NODE</key>"));
        assert!(plist.contains("<string>/Users/USER/.nvm/versions/node/v22.11.0/bin/node</string>"));
        assert!(plist.contains("<key>ERRAND_BROWSER</key>"));
        // Still a well-formed dict: PATH keeps its place alongside them.
        assert!(plist.contains("<key>PATH</key>"));
    }

    #[test]
    fn variables_that_were_not_set_do_not_appear_at_all() {
        // An empty string here would be worse than absent: it would override
        // the search with a path that cannot exist.
        let plist = render_plist_with(
            Path::new("/Applications/Errand-AI.app/Contents/MacOS/errandd"),
            Path::new("/Users/USER/Library/Application Support/com.errandai.app/logs"),
            &[],
        );
        assert!(!plist.contains("ERRAND_NODE"));
        assert!(!plist.contains("ERRAND_SIDECAR"));
        assert!(!plist.contains("ERRAND_BROWSER"));
    }

    #[test]
    fn an_ampersand_in_a_path_does_not_produce_an_unreadable_plist() {
        // launchd reports a plist it cannot parse as no agent at all, which
        // looks exactly like never having installed one.
        let plist = render_plist_with(
            Path::new("/Applications/Rock & Roll/errandd"),
            Path::new("/Users/USER/logs"),
            &[(
                "ERRAND_SIDECAR".to_string(),
                "/opt/a&b/main.mjs".to_string(),
            )],
        );
        assert!(plist.contains("/Applications/Rock &amp; Roll/errandd"));
        assert!(plist.contains("/opt/a&amp;b/main.mjs"));
        // Every ampersand in the document is part of an entity, not a bare one.
        assert_eq!(plist.matches('&').count(), plist.matches("&amp;").count());
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
