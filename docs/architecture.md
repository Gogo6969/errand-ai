# How it is built

## Two processes

**`errandd`** is a background daemon started by launchd. It owns the schedule,
runs tasks, holds the keychain access, drives AppleScript, and serves a local
HTTP API on `127.0.0.1:4477`.

**`Errand-AI.app`** is a window. It executes nothing, opens no database, and
touches no website. Every screen is an API client.

The split is what lets a task fire at eight in the morning with the window
closed, and lets you quit the window without stopping a run. It also means the
app has one job to get right: the API token, which is read in Rust and never
handed to the webview, because a token in JavaScript is a token any page script
can read, and this one can start runs and read your whole history.

The daemon is also the only writer of the database. The window never opens it,
not even read-only: a read-only connection to a WAL database needs a read-write
peer, so it fails exactly when the daemon is down and you are trying to find out
why.

## The pieces

```
  Errand-AI.app ──HTTP──► errandd ──┬──► claude (contained agent)
   (SvelteKit)              │       │
                            │       ├──► browser-agent (Node + your Chrome)
                       SQLite       │
                            │       ├──► Telegram / WhatsApp / AppleScript
                       Keychain     │
                                    └──► your AI providers
```

- **`core/`**: everything with no side effects: the schedule, the playbook
  format, the domain rules, the database, the keychain wrapper, the limits.
  Heavily tested, because it is where the decisions are.
- **`runner/`**: the daemon: scheduler, executor, MCP tool server, browser
  supervision, outbox, webhooks, API.
- **`sidecars/browser-agent/`**: a small Node process driving a Chrome-family
  browser through Playwright, speaking newline-delimited JSON over stdin.
- **`frontend/`**: SvelteKit, served from inside the app bundle.
- **`packages/client/`**: a TypeScript client for the API.

## How a run happens

1. The scheduler notices an occurrence is due and creates a run, keyed to the
   scheduled instant. That key is what makes the run idempotent.
2. The executor writes a per-run MCP config with a token good for that run only,
   then spawns the agent with an explicit tool allowlist and a runtime check
   that the surface it got is the surface it asked for.
3. The agent works through Errand's tools: read the brief, open the browser,
   look at the page, act, journal what it did. Every tool call is checked
   against the run's budget first.
4. Anything irreversible claims a fence slot before it happens and commits
   evidence after.
5. The run ends by calling `finish` or `fail`. A run that stops without saying
   which is treated as not done, never as done.

## Why the agent runs as a separate process

Because containment you can assert is worth more than containment you assume.
The agent is a child process with a tool surface Errand hands it and then
verifies. If that check fails, the run fails closed. Running the loop in-process
would mean trusting configuration; running it out-of-process means checking.

## Storage

Everything lives in `~/Library/Application Support/com.errandai.app/`:
the database, run artifacts, playbooks and logs. Secrets live in the macOS
keychain and nowhere else: never in the database, the logs, the journal,
screenshots, or a model prompt.

## Other platforms

The engine is portable; the edges are not. The keychain, launchd and AppleScript
are macOS-specific and each is behind an interface with an honest stub
elsewhere. Windows and Linux are later work, not a flag away.
