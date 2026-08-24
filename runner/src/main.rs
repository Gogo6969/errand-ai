//! `errandd`, the Errand-AI background runner.
//!
//! This is the only process that schedules, executes, journals, reads the
//! keychain, drives AppleScript, and serves the API. The Tauri app is a client
//! of this daemon, which is what makes a task fire at 08:00 with the window
//! closed.
//!
//! Subcommands:
//!   errandd --launchd        run under launchd (the normal case)
//!   errandd --foreground     run in this terminal, logging to stderr
//!   errandd install <exe>    write and load the LaunchAgent
//!   errandd uninstall        unload and remove it
//!   errandd doctor           diagnose the environment
//!   errandd token            print the primary API token

mod api;
mod lock;
mod secrets;
mod state;

use anyhow::{Context, Result};
use errand_core::launchd::ServiceManager;
use state::AppState;
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("--foreground");

    match cmd {
        "install" => {
            let exe = args
                .get(1)
                .map(std::path::PathBuf::from)
                .unwrap_or(std::env::current_exe()?);
            let plist = errand_core::launchd::Launchd.install(&exe)?;
            println!("LaunchAgent installed at {}", plist.display());
            println!("The runner now starts at login and survives the app being quit.");
            Ok(())
        }
        "uninstall" => {
            errand_core::launchd::Launchd.uninstall()?;
            println!("LaunchAgent removed. Scheduled tasks will no longer run.");
            Ok(())
        }
        "doctor" => doctor().await,
        "token" => match api::auth::read_primary_token().await {
            Ok(t) => {
                println!("{t}");
                Ok(())
            }
            Err(e) => {
                eprintln!("No API token in the keychain yet: {e}");
                eprintln!("Start the runner once and it will mint one.");
                std::process::exit(1);
            }
        },
        "--launchd" | "--foreground" | "run" => serve(cmd == "--foreground" || cmd == "run").await,
        "--version" | "-V" => {
            println!("errandd {}", errand_core::VERSION);
            Ok(())
        }
        other => {
            eprintln!("Unknown argument '{other}'.");
            eprintln!("Try: --foreground, --launchd, install, uninstall, doctor, token");
            std::process::exit(2);
        }
    }
}

fn init_logging(foreground: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("ERRAND_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    if foreground {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return None;
    }

    // A Finder-launched or launchd-launched process loses stdout, so the real
    // log has to be a file. Daily rolling, pruned by the janitor.
    match errand_core::paths::logs_dir() {
        Ok(dir) => {
            let _ = std::fs::create_dir_all(&dir);
            let appender = tracing_appender::rolling::daily(&dir, "errandd.log");
            let (nb, guard) = tracing_appender::non_blocking(appender);
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(nb)
                .init();
            Some(guard)
        }
        Err(_) => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            None
        }
    }
}

async fn serve(foreground: bool) -> Result<()> {
    let _guard = init_logging(foreground);

    // One daemon per session. A second instance exits zero so launchd does not
    // treat it as a crash and thrash.
    let Some(_lock) = lock::RunnerLock::acquire()? else {
        let pid = lock::RunnerLock::holder_pid().unwrap_or_else(|| "unknown".into());
        tracing::info!("another errandd is already running (pid {pid}); exiting cleanly");
        if foreground {
            eprintln!("errandd is already running (pid {pid}).");
        }
        return Ok(());
    };

    errand_core::paths::ensure_dirs()?;
    let pool = errand_core::db::open()
        .await
        .context("opening the database")?;

    let check = errand_core::db::quick_check(&pool).await?;
    if check != "ok" {
        tracing::error!("database integrity check failed: {check}");
        anyhow::bail!("database integrity check failed: {check}");
    }

    let state = AppState::new(pool.clone());
    let app =
        api::routes::router(state.clone()).layer(tower_http::trace::TraceLayer::new_for_http());

    let port = std::env::var("ERRAND_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(errand_core::DEFAULT_API_PORT);

    // Loopback only. LAN mode is an explicit opt-in that also makes TLS
    // mandatory, and it does not exist yet.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cannot bind {addr}: {e}");
            anyhow::bail!(
                "Port {port} is already in use, so the API cannot start. \
                 Another Errand runner may be running, or something else has taken the port."
            );
        }
    };

    tracing::info!(
        version = errand_core::VERSION,
        schema = errand_core::SCHEMA_VERSION,
        %addr,
        "errandd listening"
    );
    if foreground {
        println!(
            "errandd {} listening on http://{addr}",
            errand_core::VERSION
        );
    }

    // Only now touch the keychain.
    //
    // This ordering is load-bearing. A keychain call can block behind an
    // authorization prompt that has no window to appear in under launchd, and a
    // daemon that mints its token before binding its port hangs forever with no
    // log and no way to ask it what is wrong. With the listener already up,
    // `GET /v1/health` answers and reports the keychain as blocked instead.
    let boot_state = state.clone();
    tokio::spawn(async move {
        match api::auth::ensure_primary_token(&pool).await {
            Ok(Some(token)) => {
                tracing::info!("minted primary API token (stored in your keychain)");
                if foreground {
                    println!(
                        "\n  Primary API token (also saved to your keychain):\n\n    {token}\n"
                    );
                    println!("  Retrieve it later with: errandd token\n");
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("could not mint the primary API token: {e}");
                if foreground {
                    eprintln!("\n  Could not mint the primary API token.\n  {e}\n");
                }
            }
        }
        let ks = secrets::probe().await;
        boot_state.set_keychain(ks);
        if ks != secrets::KeychainState::Ok {
            tracing::warn!(
                state = ks.as_str(),
                "keychain is not answering normally; credentials will not work until this is fixed"
            );
        }
    });

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("interrupt received, shutting down");
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("serving the API")?;
    Ok(())
}

/// One command that answers "why is my agent not working". Every check prints
/// what it found and what to do about it, never just a status.
async fn doctor() -> Result<()> {
    println!(
        "Errand-AI doctor ({} / schema {})\n",
        errand_core::VERSION,
        errand_core::SCHEMA_VERSION
    );

    let mut problems = 0;

    // Paths
    match errand_core::paths::data_root() {
        Ok(p) => {
            let exists = p.exists();
            println!("  {}  data directory: {}", tick(exists), p.display());
            if !exists {
                println!("      It will be created the first time the runner starts.");
            }
        }
        Err(e) => {
            problems += 1;
            println!("  x  data directory: {e}");
        }
    }

    // Translocation
    if let Ok(exe) = std::env::current_exe() {
        let bad = errand_core::paths::is_translocated(&exe);
        println!("  {}  binary location: {}", tick(!bad), exe.display());
        if bad {
            problems += 1;
            println!("      This is a temporary path. Move Errand-AI to /Applications, or the");
            println!("      background runner will stop working after the next login.");
        }
    }

    // launchd
    let loaded = errand_core::launchd::Launchd.is_loaded();
    println!(
        "  {}  launchd agent {}",
        tick(loaded),
        errand_core::LAUNCHD_LABEL
    );
    if !loaded {
        println!("      Not loaded, so nothing runs on a schedule. Install it with:");
        println!("      errandd install");
    }

    // Database
    match errand_core::db::open().await {
        Ok(pool) => {
            let check = errand_core::db::quick_check(&pool)
                .await
                .unwrap_or_default();
            let ok = check == "ok";
            if !ok {
                problems += 1;
            }
            println!("  {}  database integrity: {check}", tick(ok));
            let busy = errand_core::db::count_busy_runs(&pool).await.unwrap_or(0);
            println!("  -  runs in flight: {busy}");
        }
        Err(e) => {
            problems += 1;
            println!("  x  database: {e}");
        }
    }

    // Keychain. Time-bounded, because the interesting failure is a blocked
    // authorization prompt rather than a clean error.
    {
        let ks = secrets::probe().await;
        let ok = ks == secrets::KeychainState::Ok;
        if !ok {
            problems += 1;
        }
        println!("  {}  keychain read/write: {}", tick(ok), ks.as_str());
        match ks {
            secrets::KeychainState::Blocked => {
                println!("      macOS is waiting on an authorization prompt for an item whose");
                println!("      access list no longer matches this build. Open Keychain Access,");
                println!("      delete the 'com.errandai.app' items, and start the runner again.");
            }
            secrets::KeychainState::Error => {
                println!(
                    "      Errand cannot store credentials, so no task can log into anything."
                );
            }
            secrets::KeychainState::Ok => {}
        }
    }

    // API token
    let has_token = api::auth::read_primary_token().await.is_ok();
    println!("  {}  API token in keychain", tick(has_token));
    if !has_token {
        println!("      Start the runner once and it will mint one.");
    }

    // Claude CLI: the flagship executor
    let claude = find_claude();
    match &claude {
        Some(p) => {
            let version = std::process::Command::new(p)
                .arg("--version")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            println!("  {}  claude CLI: {} {}", tick(true), p.display(), version);
        }
        None => {
            println!("  !  claude CLI: not found");
            println!("      The default executor needs it. Checked ~/.local/bin, /usr/local/bin,");
            println!("      /opt/homebrew/bin, and PATH.");
        }
    }

    // API reachable
    let port = errand_core::DEFAULT_API_PORT;
    let reachable = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok();
    println!("  {}  API on 127.0.0.1:{port}", tick(reachable));
    if !reachable {
        println!("      Nothing is listening. The runner is not up.");
    }

    println!();
    if problems == 0 {
        println!("No problems found.");
    } else {
        println!("{problems} problem(s) above need attention.");
    }
    Ok(())
}

fn tick(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "x "
    }
}

/// Resolve the claude binary the same way CCC does.
fn find_claude() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_BIN") {
        let p = std::path::PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".local/bin/claude"),
        std::path::PathBuf::from("/usr/local/bin/claude"),
        std::path::PathBuf::from("/opt/homebrew/bin/claude"),
    ];
    candidates.into_iter().find(|p| p.exists()).or_else(|| {
        std::process::Command::new("which")
            .arg("claude")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string())
            })
            .filter(|p| p.exists())
    })
}
