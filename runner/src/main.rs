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
mod browser;
mod channels;
mod executor;
mod fixer;
mod lock;
mod mcp;
mod models;
mod outbox;
mod planner;
mod redact;
mod scheduler;
mod secrets;
mod state;
mod webhooks;

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
        "doctor" => {
            let problems = doctor().await?;
            // Exit explicitly rather than returning.
            //
            // A keychain call that stalls behind an authorization prompt is
            // running on a blocking thread, and a timeout only abandons the
            // future: tokio cannot cancel the thread, so the runtime would wait
            // for it forever and the command would hang after having already
            // printed its answer. Leaving is the correct end of a diagnostic.
            std::process::exit(i32::from(problems > 0));
        }
        "token" if args.get(1).map(String::as_str) == Some("--new") => {
            let pool = errand_core::db::open().await?;
            match api::auth::regenerate_primary_token(&pool).await {
                Ok(t) => {
                    println!("{t}");
                    eprintln!(
                        "\nThe previous token has been revoked. Update anything that used it."
                    );
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Could not mint a replacement token: {e}");
                    std::process::exit(1);
                }
            }
        }
        "token" => match api::auth::read_primary_token().await {
            Ok(t) => {
                println!("{t}");
                // Same reason as doctor: a wedged keychain thread must not keep
                // the process alive after the answer has been given.
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Cannot read the API token: {e}");
                eprintln!("If the runner has never started, start it and it will mint one.");
                eprintln!("Otherwise mint a replacement with: errandd token --new");
                std::process::exit(1);
            }
        },
        "--launchd" | "--foreground" | "run" => serve(cmd == "--foreground" || cmd == "run").await,
        "--version" | "-V" => {
            println!("errandd {}", errand_core::VERSION);
            Ok(())
        }
        "--help" | "-h" | "help" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("Unknown argument '{other}'.\n");
            print_help();
            std::process::exit(2);
        }
    }
}

/// Every command, and what it is actually for. The product's rule is that
/// nothing goes unexplained, and that starts at the command line.
fn print_help() {
    println!(
        r#"errandd {version} - the Errand-AI background runner.

This is the process that does the work. It schedules tasks, runs them with a
contained AI agent, records what happened, holds your credentials, and serves
the local API. It keeps running whether or not any window is open.

USAGE
  errandd <command>

RUNNING
  --launchd          Run as the background agent. This is how launchd starts it;
                     you would not normally type it yourself.
  --foreground       Run here in this terminal, logging to the screen. Use this
                     when you want to watch what it is doing.

SETUP
  install [path]     Install it as a background agent that starts at login.
                     Point this at a stable copy of the binary, never at a build
                     directory: rebuilding a running binary deadlocks it on macOS.
  uninstall          Remove the background agent. Scheduled tasks stop running.

WHEN SOMETHING IS WRONG
  doctor             Check everything that has to be true for a task to run, and
                     say what to do about anything that is not.
  token              Print the API token, for talking to the local API.
  token --new        Mint a replacement and revoke the old one. Use this when
                     the stored token can no longer be read.
  --version          Print the version.

THE LOCAL API
  http://127.0.0.1:4477 - loopback only. Use 127.0.0.1 rather than localhost,
  because the listener is IPv4 and localhost can resolve to IPv6 first.

    curl -H "Authorization: Bearer $(errandd token)" \
         http://127.0.0.1:4477/v1/tasks

DATA
  Everything lives in ~/Library/Application Support/com.errandai.app/
  Secrets live in your macOS keychain and nowhere else."#,
        version = errand_core::VERSION
    );
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

    // Before anything opens a socket. launchd's default of 256 open files is
    // fine for ordinary work and nowhere near enough to sweep a network.
    models::raise_file_descriptor_limit();

    errand_core::paths::ensure_dirs()?;
    let pool = errand_core::db::open()
        .await
        .context("opening the database")?;

    let check = errand_core::db::quick_check(&pool).await?;
    if check != "ok" {
        tracing::error!("database integrity check failed: {check}");
        anyhow::bail!("database integrity check failed: {check}");
    }

    let port = std::env::var("ERRAND_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(errand_core::DEFAULT_API_PORT);

    let state = AppState::new(pool.clone());
    state.set_api_port(port);
    let app =
        api::routes::router(state.clone()).layer(tower_http::trace::TraceLayer::new_for_http());

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

    // The scheduler is what makes a task fire without anyone asking. It starts
    // after the listener so a scheduling problem can never stop the API from
    // answering questions about why nothing is running.
    // Which AI to use is a question the Settings screen must be able to answer,
    // so the list is seeded and checked at boot rather than the first time
    // something needs a model.
    {
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Err(e) = models::ensure_builtin(&pool).await {
                tracing::warn!("could not record the built-in AI: {e}");
            }
            if let Err(e) = models::refresh_health(&pool).await {
                tracing::warn!("could not check the AI providers: {e}");
            }
        });
    }

    scheduler::spawn(state.clone());
    outbox::spawn(state.clone());
    channels::inbound::spawn(state.clone());
    webhooks::spawn(state.clone());

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
                tracing::info!("minted a primary API key and saved it beside the database");
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
async fn doctor() -> Result<u32> {
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
        println!(
            "  {}  secrets kept in: {}",
            tick(ok),
            errand_core::keychain::store_description()
        );
        println!("  {}  read/write: {}", tick(ok), ks.as_str());
        match ks {
            secrets::KeychainState::Blocked => {
                println!("      macOS is waiting on an authorization prompt for an item whose");
                println!("      access list no longer matches this build. Open Keychain Access,");
                println!("      delete the 'com.errandai.app' items, and start the runner again.");
            }
            #[allow(unreachable_patterns)]
            _ if !errand_core::keychain::using_keychain() => {
                println!("      This is a development build, so its secrets are in a plain file");
                println!(
                    "      rather than your keychain. That is deliberate: macOS ties keychain"
                );
                println!("      access to a code signature, and every rebuild produces a new one,");
                println!("      so a development build would ask permission on every compile.");
                println!("      Do not keep anything you actually care about in this build.");
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
    if !has_token {
        problems += 1;
    }
    println!("  {}  API key saved", tick(has_token));
    if !has_token {
        println!("      The API itself still works: what authenticates a request is the hash");
        println!("      in the database, and the saved copy is only there so the window and the");
        println!("      command line can read it back.");
        println!("      Mint one with 'errandd token --new', or just start the runner.");
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
            problems += 1;
            println!("  !  claude CLI: not found");
            println!("      The default executor needs it. Checked ~/.local/bin, /usr/local/bin,");
            println!("      /opt/homebrew/bin, and PATH.");
        }
    }

    // Browsing: the sidecar script, the Node that runs it, and the browser it
    // would drive. Three separate things, each of which fails on its own and
    // each of which used to surface as the same shrug at 08:00.
    problems += browsing_checks().await;

    // API reachable.
    //
    // The same port the runner would bind, ERRAND_API_PORT included. A doctor
    // that checked a port nothing was using would report the service down while
    // it was running perfectly well on another one.
    let port: u16 = std::env::var("ERRAND_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(errand_core::DEFAULT_API_PORT);
    let reachable = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok();
    println!("  {}  API on 127.0.0.1:{port}", tick(reachable));
    if !reachable {
        println!("      Nothing is listening. The runner is not up.");
    }

    // Does the token actually WORK?
    //
    // Reading one back is not the same as it being accepted, and the difference
    // is not academic: the saved copy and the hash in the database can drift
    // apart: switching between a debug and a release build does it, because
    // they keep their secrets in different places. Doctor used to report the
    // token as fine in exactly that case, while every window got a 401. A clean
    // bill of health that is not true is worse than no check at all.
    if reachable && has_token {
        let accepted = match api::auth::read_primary_token().await {
            Ok(t) => reqwest::Client::new()
                .get(format!("http://127.0.0.1:{port}/v1/health/detail"))
                .bearer_auth(t)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false),
            Err(_) => false,
        };
        if !accepted {
            problems += 1;
        }
        println!("  {}  that token is accepted", tick(accepted));
        if !accepted {
            println!("      The saved key and the running service no longer agree, so every");
            println!("      window will be refused. Nothing is lost; the key is only a copy.");
            println!("      Mint a matching one:");
            println!();
            println!("        errandd token --new");
            println!();
            println!("      Then reopen the Errand-AI window.");
        }
    }

    println!();
    if problems == 0 {
        println!("No problems found.");
    } else {
        println!("{problems} problem(s) above need attention.");
    }
    Ok(problems)
}

/// The three things that all have to be true before a task can open a web
/// page, checked separately because they fail separately. Returns the number
/// that are not true.
async fn browsing_checks() -> u32 {
    let mut problems = 0;

    let script = browser::sidecar_script();
    match &script {
        Ok(p) => println!("  {}  browser helper: {}", tick(true), p.display()),
        Err(e) => {
            problems += 1;
            println!("  x  browser helper: not found");
            print_fix(&e.to_string());
        }
    }

    let node = browser::which_node();
    match &node {
        Some(p) => {
            let version = std::process::Command::new(p)
                .arg("--version")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            // Playwright will not run on an old one, and the failure it gives
            // is a syntax error from deep inside a library. Decided before the
            // line is printed, so the line does not say ok about a node that
            // is about to be told off underneath it.
            let too_old = node_major(&version).is_some_and(|m| m < 20);
            println!("  {}  node: {} {}", tick(!too_old), p.display(), version);
            if too_old {
                problems += 1;
                print_fix(&format!(
                    "This is {version}, and the browser needs Node 20 or newer. Upgrade it, or \
                     set ERRAND_NODE to the full path of a newer copy. Until then web pages will \
                     not open; everything else runs as normal."
                ));
            }
        }
        None => {
            problems += 1;
            println!("  x  node: not found");
            print_fix(browser::NODE_MISSING_HELP);
        }
    }

    // Which browser would actually be driven. Worth asking only when there is
    // something to ask with, and the answer comes from the sidecar because it
    // is the layer that knows where it looks.
    match (&script, &node) {
        (Ok(script), Some(node)) => match browser::probe_browser(node, script).await {
            Ok(p) if p.found => println!(
                "  {}  browser to drive: {}",
                tick(true),
                p.name.as_deref().unwrap_or("a Chrome-family browser")
            ),
            Ok(p) => {
                problems += 1;
                println!("  x  browser to drive: none installed");
                print_fix(p.message.as_deref().unwrap_or(
                    "The browser helper found no Chrome-family browser on this Mac. Install \
                     Google Chrome from https://www.google.com/chrome and run this again.",
                ));
            }
            Err(e) => {
                problems += 1;
                println!("  x  browser to drive: could not be asked");
                print_fix(&format!(
                    "Errand started the browser helper to ask which browser it would use, and it \
                     did not answer: {e}. Nothing was opened. This usually means the helper's own \
                     installation is incomplete, so reinstalling Errand is the fix."
                ));
            }
        },
        _ => println!("  -  browser to drive: not checked, because of the problem above"),
    }

    problems
}

/// The major version out of something like `v22.11.0`.
fn node_major(version: &str) -> Option<u32> {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// Print the fix under a failing line, wrapped to the width the rest of doctor
/// is hand-wrapped to. Every failure gets one of these: a status on its own
/// tells a worried person nothing they can act on.
fn print_fix(text: &str) {
    const WIDTH: usize = 72;
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > WIDTH {
            println!("      {line}");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        println!("      {line}");
    }
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
