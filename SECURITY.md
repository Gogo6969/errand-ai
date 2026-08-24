# Security

Errand-AI logs into your accounts, unattended, on a schedule, while reading text written by
strangers. The threat model is not theoretical, so this document states what the software
guarantees and what it deliberately refuses to do.

## Reporting a vulnerability

Open a private security advisory through the repository's Security tab. Please include what you
did, what happened, and what you expected. Expect a first response within a week. Only the latest
release is supported.

## Properties this software upholds

Any path that breaks one of these is a security bug, not a feature request.

1. **Secrets never leave the keychain except to be used.** A credential is read at the moment of
   use, held in one stack frame, written once to the browser, and zeroed. It never enters an AI
   prompt, the database, the run journal, the logs, a screenshot, or any API response at any
   permission level.
2. **A credential is bound to one domain.** It is released only to the exact apex domain it was
   registered against. A page that merely looks like that domain gets nothing.
3. **The agent has one tool surface.** The executor is given the app's own tool server and nothing
   else: no shell, no filesystem, no arbitrary fetch. Page-derived text is treated as untrusted
   data, never as instructions.
4. **The agent cannot wander.** Navigation is confined to the domains the task declares.
5. **One scheduled occurrence produces one irreversible action.** The fence is scoped to the
   occurrence rather than to the outcome, so a retry that chooses differently cannot slip past it.
6. **Starting a run and approving one are different permissions.** `approve` is a separate scope
   precisely so an automated client cannot confirm what it started.
7. **The API is loopback-only by default** and never calls the public internet.
8. **The app never phones home.** No telemetry, no analytics, no account.

## What it will not automate, deliberately

CAPTCHAs are never solved or bypassed. Payment card numbers are never stored or typed. SMS codes,
passkeys, and hardware keys stop the run and ask you to take over. These are policy, not gaps.

## Known limitations

- **Cloud AI providers see page content.** If you route a task through a cloud model, the pages it
  reads and the screenshots it takes are part of what that provider receives. Redaction removes
  known secret values, not every personal detail. Use a local model for sensitive tasks.
- **Development builds and the keychain.** Debug binaries are re-signed on each build, so keychain
  items created by a previous build no longer match and macOS will prompt. The daemon bounds every
  keychain call with a timeout and reports the state rather than hanging, but you may need to clear
  `com.errandai.app` items after a rebuild.
- **Advisory locking only.** The single-instance guarantee relies on an advisory file lock, which
  protects against accident rather than against a determined local process.
