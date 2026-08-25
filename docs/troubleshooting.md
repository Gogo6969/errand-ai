# When something is wrong

Start here:

```bash
errandd doctor
```

It checks everything that has to be true for a task to run — the data
directory, launchd, the database, the keychain, the API token, the port, the
Claude command line tool, Node, the browser sidecar and which browser would be
driven — and prints the fix for anything that is not. Every failing line says
what to do, not just what is broken.

## The window says the background service is not answering

The daemon is not running, or is not reachable on `127.0.0.1:4477`.

```bash
launchctl kickstart -k gui/$(id -u)/com.errandai.runner
```

If it comes straight back down, run `errandd --foreground` in a terminal and
read what it says. The usual causes are a port already in use and a database
that failed its integrity check.

## Errand keeps asking for keychain permission

macOS ties keychain access to a program's *code signature*, and `cargo build`
relinks the binary every time, so each compile looks like a different program.
That is why "Always Allow" never sticks for a build you made yourself.

Errand handles this by not putting a development build's secrets in your
keychain at all:

- A **release** build — the app, and what `scripts/dev-install.sh` installs —
  uses the keychain and is signed with a stable identity. macOS asks once and
  the answer holds.
- A **debug** build — `cargo run`, `cargo test`, `scripts/smoke.sh` — keeps its
  secrets in `dev-secrets.json` beside its own database, readable only by you.
  It never raises a prompt. `errandd doctor` says which of the two is in use.

Override it either way with `ERRAND_KEYCHAIN=on` or `ERRAND_KEYCHAIN=off`.

If you are still being asked, it is an old item whose access list no longer
matches anything. Clear the development ones — they hold nothing you need:

```bash
security delete-generic-password -s com.errandai.app.internal.dev
```

Repeat until it says the item cannot be found, and do the same for
`com.errandai.app.credentials.dev`. Leave the entries without `.dev` alone:
those belong to the real app.

## A task says it cannot open a website

Almost always the allowlist. A task with no sites can open nothing, and the
match is one-directional, so `www.example.com` does not cover `example.com`.
Check the list on the task's page and add the plain form of the site.

If the message names a site that *is* on the list, the page redirected somewhere
that is not — which is the allowlist doing its job.

## Nothing happens at the scheduled time

Four things have to be true, in this order:

1. The task is **Armed** — not a draft, not paused.
2. It has an **approved plan**. Nothing runs unattended without one.
3. Its schedule is a real schedule. The task page shows what it means in words
   and the next few run times; if that says "only when you ask", it is manual.
4. The daemon is running. A Mac that was asleep runs the task late instead,
   unless the schedule says to skip missed runs.

## No message ever arrives

Errand needs somewhere to send it. Until Telegram has a bot token and a chat id
in Settings, it has nowhere to send results and says nothing — which looks
exactly like a task that never ran. `errandd doctor` reports this.

Messages to *other people* also wait for quiet hours to end. Your own failure
alerts do not, unless you turned that off.

## The browser will not start

Three separate causes, and doctor distinguishes them:

- **No browser.** Errand drives an installed Chrome-family browser rather than
  downloading a 300 MB one. Install Google Chrome, or point `ERRAND_BROWSER` at
  the executable inside the browser you prefer. It uses a profile of its own, so
  your windows, tabs, history and saved logins are untouched.
- **No Node.** The browser process needs Node 20 or newer. If you installed it
  with nvm, volta or fnm, Errand looks in those places too; if it still cannot
  find it, set `ERRAND_NODE` to the path.
- **The sidecar is missing.** In a built app this means the bundle is
  incomplete. Building from source, run
  `npm --prefix sidecars/browser-agent install`.

## A task is paused and says it needs me

A run began something that cannot be undone and stopped before confirming it.
Errand will not guess: booking again and assuming it worked are both wrong.
Check the site yourself, then tell it what you found on the task page. It picks
up from there.

## A run cost far more than usual

A run that costs much more than usual is normally stuck rather than busy. Open
it and read the timeline — a loop is obvious from three lines that repeat. Lower
the step or minute limit on that task, and consider whether the description is
specific enough about what to do when the obvious path is not there.

## Where the logs are

```
~/Library/Application Support/com.errandai.app/logs/
```

Secrets are scrubbed before anything is written. If you find one there, that is
a bug worth reporting — see [SECURITY.md](../SECURITY.md).
