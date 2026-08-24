# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Cargo workspace** with `errand-core` (shared library) and `errand-runner` (the `errandd`
  daemon binary).
- **Canonical database schema** (`0001_init.sql`). One vocabulary shared by the database, the API
  wire objects, the SSE events, and webhooks, with no translation layer. Two invariants are
  enforced in SQL rather than by convention: a scheduled occurrence can produce only one run, and
  a run recorded as failed must carry both a machine-readable cause and a plain-language
  explanation.
- **`errandd` daemon** installable as a launchd agent, surviving the app being quit and restarting
  after a crash. Guards against Gatekeeper app translocation, writes only absolute paths into the
  plist, and uses `KeepAlive: {SuccessfulExit: false}` so a clean exit stays down for the update
  handover.
- **Keychain integration.** Site credentials and app secrets in the macOS login keychain. A
  `Secret` type that refuses to print itself and zeroes on drop. The database stores metadata only.
- **Local API** on `127.0.0.1:4477`: health, task CRUD, pause and resume, run triggering, run
  journals, credential management, and a clean-exit quiesce endpoint for the updater.
- **Scoped bearer tokens.** `read`, `run`, `webhook`, `approve`, `manage`, `admin`, with `approve`
  deliberately separate from `run` so a client that can start an irreversible action cannot
  confirm it. Plaintext lives in the keychain; the database stores only a hash.
- **Server-Sent Events**, both a per-run stream and a global firehose, using the same event names
  as everything else.
- **`errandd doctor`**, a single command that diagnoses the data directory, binary location,
  launchd agent, database integrity, keychain, API token, Claude CLI, and API reachability, and
  says what to do about each problem it finds.
- **Scrub gates** guarding the first commit rather than the first release: private network
  addresses, credential-shaped strings, personal identifiers, and hardcoded home paths all fail
  the push, with gitleaks over full history in CI.

### Fixed

- The daemon no longer binds its keychain before its port. A keychain call can block behind a macOS
  authorization prompt that has no window to appear in under launchd, which previously wedged the
  daemon with no log and no way to diagnose it. The listener now comes up first, every keychain
  call is time-bounded, and `GET /v1/health` reports the keychain as blocked instead of the whole
  process appearing to hang.
