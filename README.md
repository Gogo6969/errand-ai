<div align="center">

# Errand-AI

**An AI that learns a chore once, then does it on schedule, and tells you plainly when it cannot.**

*You describe a task in plain language. You watch the agent do it once. It writes itself a
playbook. After that it runs at a date and time, on a recurrence, or on demand: logging into
sites, deciding as it goes, repairing itself when a page changes, and reporting the outcome.*

[![License: MIT](https://img.shields.io/badge/License-MIT-00A97F.svg)](LICENSE)
[![Tauri 2](https://img.shields.io/badge/Tauri-2-1E3A8A.svg)](https://tauri.app/)
[![Rust](https://img.shields.io/badge/Rust-stable-CE422B.svg)](https://www.rust-lang.org/)
[![macOS](https://img.shields.io/badge/macOS-in%20development-F59E0B.svg)](#status)

</div>

> [!WARNING]
> **Early development.** Milestone 6 of 10. A contained agent learns a task from one supervised
> run, repeats it on a schedule, logs in with a stored credential, adapts when the site changes,
> and repairs itself within a budget, then tells you what happened over Telegram. There is no user
> interface yet, so tasks are written through the API.

---

## What this is

Not a cron scheduler. Every run is executed by an AI that reads your task description and
decides what to do, which is what lets it cope with a site that moved a button since last week.

The loop is: **describe, teach, approve, repeat.**

1. **Describe.** You write the task the way you would explain it to a person.
2. **Teach.** A visible browser opens and the agent attempts the job while narrating. You can
   pause, take over, or correct it mid-run.
3. **Approve.** The agent distils what it learned into a playbook, in markdown, that you read and
   approve. Nothing is ever scheduled before you approve it.
4. **Repeat.** It runs on your schedule. When a page changes it tries to repair its own approach.
   When it genuinely cannot finish, it stops and tells you what it was doing, why it could not,
   and what you can do about it.

## Design commitments

These are properties the code is built to hold, not aspirations:

- **Secrets live in the macOS keychain.** Never in the database, never in a log, never in a
  screenshot, never in an AI prompt, and never returned by any API response at any permission
  level. Credentials are released only to the exact domain they are bound to, which is also what
  stops a lookalike page from collecting them.
- **A failure always explains itself.** The database rejects a run recorded as failed that does
  not carry both a machine-readable cause and a plain-language explanation. That is a schema
  constraint, not a convention.
- **A scheduled occurrence produces one run, and one irreversible action.** The run is enforced by
  a unique index; the action by a fence keyed on the occurrence rather than on what the agent chose
  to do, so a retry that picks differently cannot slip past it. A run that dies between starting an
  action and confirming it blocks the task and asks you what you found, rather than guessing.
- **The agent is contained, and it is checked rather than trusted.** It reads hostile web pages
  unattended, so it gets one tool surface and an empty working directory. The tool list is
  inspected before the model can act, and anything unexpected kills the run rather than
  proceeding. Verified by deliberately removing the restrictions and watching it refuse.
- **The API is the app's own transport.** The interface is a client of the same API a third party
  would use, which is the only reliable way to keep an integration API honest.

## Architecture

Two processes, one owner of the database.

| Component | What it does |
|---|---|
| `Errand-AI.app` | Tauri 2 shell and menu bar presence. Never executes a task. A pure API client. |
| `errandd` | Headless daemon under launchd. Schedules, executes, journals, holds the keychain, serves the API. Runs whether or not the window is open. |

The daemon owns every permission on purpose: macOS binds Automation consent and keychain access
to the code identity that asks, so if the interface asked, a run at 03:00 would stall behind a
prompt nobody is awake to answer.

## Status

| Milestone | Delivers | State |
|---|---|---|
| M1 | Workspace, schema, daemon under launchd, keychain, REST and SSE core | **done** |
| M2a | Claude CLI executor with containment, tool server, live journal | **done** |
| M2b | Browser sidecar, symbolic actions, credential fill, redaction | **done** |
| M3 | Scheduler, run windows, catch-up, the side-effect fence | **done** |
| M4 | Teach mode and playbooks | **done** |
| M5 | Self-heal and the failure surface | **done** |
| M6 | Telegram, WhatsApp, Apple Mail, Apple Messages | **done** |
| M7 | API hardening, scoped tokens, webhooks, TypeScript client | next |
| M8 | The interface, with every control explained | |
| M9 | Open-source preparation and signing from CI | |
| M10 | Public v0.1.0 | |

## Building

Requires a stable Rust toolchain and macOS.

```bash
cargo test --workspace       # 129 tests, including a real keychain round-trip
cargo build
```

Run the daemon in your terminal:

```bash
cargo run -p errand-runner -- --foreground
```

Install it as a background agent that starts at login:

```bash
./target/debug/errandd install "$(pwd)/target/debug/errandd"
```

Diagnose anything that is not working:

```bash
./target/debug/errandd doctor
```

## The API

Loopback only, on `http://127.0.0.1:4477`. Bearer token, minted on first boot into your keychain.

```bash
TOKEN=$(errandd token)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:4477/v1/tasks
```

Tokens are rows rather than a singleton, so each client gets only the scopes it needs:
`read`, `run`, `webhook`, `approve`, `manage`, `admin`. `approve` is deliberately separate from
`run`, because approval gates exist to put a human in front of an irreversible action, so a
client that can start a booking must not be able to confirm it.

> Use `127.0.0.1` rather than `localhost`. The listener is IPv4 loopback, and `localhost` resolves
> to IPv6 first on some systems.

## Privacy

Everything is local. No account, no telemetry, no phoning home. Three things leave your machine,
and only these: prompts and page content sent to whichever AI provider you chose (nothing at all
if that is a local model on your own network), the web traffic of the task itself, and messages
you configured it to send.

Screenshots and page content from sites you are logged into are part of what a cloud provider
sees. Redaction removes known secrets, not every personal detail. For sensitive tasks, choose a
local model.

## Licence

MIT. See [LICENSE](LICENSE).
