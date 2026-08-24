# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## M3

### Added

- **The scheduler.** Tasks now fire on their own. Occurrences are computed in the task's IANA
  timezone with instants stored in UTC, so the same wall-clock time is a different moment either
  side of a daylight-saving change rather than drifting by an hour twice a year. A local time that
  never existed runs just after the gap; an ambiguous one takes the first occurrence.
- **Run windows.** A task can start early enough to be logged in and waiting before the moment it
  cares about, and abandons an occurrence whose window has closed rather than doing it late.
- **Catch-up.** Occurrences missed while the machine slept or the daemon was down follow the task's
  policy: skip, run once if still inside the grace window, or replay all. The scheduler's position
  is persisted, so a gap caused by a restart is seen on the next boot rather than being invisible.
- **The side-effect fence, wired to the agent.** Any click on a control that books, pays, sends or
  deletes is classified in Rust from what the control actually says, consulted against the fence,
  and committed with evidence only after it succeeds. One occurrence admits one such action.
- **Crash recovery.** Runs left mid-flight by a killed daemon are closed at boot with an
  explanation, and a run interrupted while holding an unresolved irreversible action pauses its
  task rather than letting it quietly repeat.
- **Task activation.** `POST /v1/tasks/{id}/activate` moves a task from draft to ready.

### Fixed

Found by an adversarial review of this milestone, which returned thirteen blockers. The ones that
mattered most:

- **The fence was decorative.** `arm_side_effect` had tests and no callers. Nothing consulted it
  before an irreversible action, so the central safety claim was enforced by nothing.
- **A manual run minted a fresh occurrence every time**, so it could never see the fence belonging
  to the scheduled run it was repeating. The ordinary sequence of a run dying mid-booking and the
  user pressing Run now would have booked a second court.
- **No task could ever reach `ready` through any product path.** Only raw SQL could activate one,
  which is how it went unnoticed: the scheduler was being tested through a back door.
- **The fence was a check-then-write with no transaction**, so two callers could both claim an
  aborted slot and both receive a go-ahead. It is now a single statement.
- **`commit_side_effect` returned success when it wrote nothing**, losing the evidence at the exact
  moment it mattered.
- **Every error from run creation was read as "already ran"**, so a busy or failing database would
  drop an occurrence with no run, no failure and no record.
- **Occurrence truncation kept the oldest**, discarding exactly the recent occurrences catch-up
  needs and leaving a task silently not running late when its policy said it should.
- **`start_at` and jitter were display-only.** The interface showed a start time the run provably
  would not use, and a window's early start never happened.
- **A window straddling local midnight was always missed**, abandoning every occurrence with an
  explanation that was untrue.

## M4

### Added

- **Playbooks.** What the agent learned, written as markdown, versioned on disk and indexed in
  SQLite. Plain markdown rather than a JSON blob in a column, because a structure nobody can read
  is a structure nobody checks: you can open it, diff two versions, and see exactly what your
  agent believes about a site.
- **Intent and hint are separate.** A step records what it was trying to achieve and, separately,
  how that happened to work last time. Intents survive a site redesign; hints do not, and the
  agent is told to pursue the intent when a hint stops matching the page.
- **Teach mode.** `POST /v1/tasks/{id}/teach` runs a task from its description alone, and the
  agent calls `save_playbook` near the end with what actually worked.
- **The approval gate.** A new playbook is stored unapproved and is followed by nothing. A task
  with no approved playbook cannot be run normally and cannot be put on a schedule at all, because
  scheduling one would send an unattended agent at a site with no agreed way of doing the job.
  Approving needs the approve scope, not merely the one that starts runs.
- **Only hints change themselves.** A playbook is distilled from pages written by strangers and
  then fed back as trusted instruction, so the review gate is what stands between a hostile page
  and a permanent foothold. Anything that changes the goal, a step's intent, a decision rule, or a
  "Never do" line waits for a person.
- **Notes between runs.** `leave_note` records something small a run learned, and the next run
  reads the last few, oldest first.

### Fixed

- **The action classifier treated navigation as commitment.** Found by testing against a real
  site: a homepage link reading "Book a court" was classified as a booking, which armed the fence
  and then blocked the actual booking button on the next page, so the task could never book at
  all. A link that merely navigates is no longer a commitment however it is worded, buttons that
  submit a form are marked as such at the point of capture, and an unrecognised form submission
  errs toward being treated as a commitment.
- **Runs were ordered by a second-resolution timestamp alone**, so two runs in the same second
  came back in an arbitrary order. The notes fed to an agent could be the wrong way round.

## M5

### Added

- **The retry ladder.** A failure is judged by its taxonomy rather than by how it felt. Something
  transient is simply tried again; a page that changed gets a diagnosis first and then one attempt
  with new advice; a wall is never retried at all, because trying again would hit the same wall.
- **The Fixer.** A small, tool-less model call that reads the tail of the journal and answers two
  questions: what went wrong, and what to try instead. Its advice is handed to the next attempt as
  a lead rather than as fact, with instructions to believe the page over the advice and to stop
  rather than try a third variation.
- **Repair is a fresh run, not a resumption.** That avoids reasoning about half-finished state, and
  it is only safe because of the side-effect fence: a repeat cannot redo anything irreversible the
  failed attempt already committed. Verified against a site that fails its first request, where
  the ladder retried the whole run and the booking still happened exactly once.
- **The Fixer never edits the playbook.** It advises one attempt. A failure diagnosed from a
  hostile page is not a trustworthy source of permanent instruction, so anything worth keeping goes
  through the same human approval as everything else.
- **Budgets.** Steps, wall clock, spend, heal cycles, and messages to real people. A breach ends
  the run naming the ceiling it hit and what to do about it, because "budget exceeded" on its own
  tells nobody anything they can act on.
- **Auth failures pause the task** rather than failing the same way every morning until somebody
  notices.

### Fixed

- **Budgets were a post-mortem rather than a budget.** The ceiling was only checked once the run
  had already stopped, so a task with a four-step limit ran fourteen steps before anyone noticed.
  It is now checked before every tool call, and the run winds down at seven. The tools that end a
  run stay open, so an over-budget agent can still report how far it got.

## M6

### Added

- **Telegram, the channel Errand relies on.** A run finishing produces a card saying what
  happened, how long it took and what it cost. WhatsApp rides an unofficial gateway that decays
  and the Apple channels need consent that can be revoked, so run outcomes go here.
- **A read-only bot.** You can ask it `/status`, `/tasks` and `/runs` from your phone. It cannot
  start a run, approve anything, or change a task, because a chat message is not a good place to
  authorise something that cannot be undone.
- **Every inbound message is checked against one configured owner** and dropped otherwise. A bot
  is addressable by anyone who discovers its username, and the token protects sending rather than
  receiving. With no owner set, nobody is trusted rather than everybody.
- **WhatsApp, honestly described.** There is no official API for personal accounts, so this drives
  WhatsApp Web through a gateway. Sessions log out and need a QR rescan that cannot happen while
  you are asleep, and automated sending can get a personal number banned. It is best effort, off
  by default, and never the only way you find out what happened.
- **Apple Mail and Apple Messages** through AppleScript, with the two error codes that actually
  matter translated into something you can act on rather than passed through as numbers.
- **Consent is requested from a button**, not from a run. macOS grants Automation to whichever
  process sends the Apple Event, so the daemon asks while you are looking at the screen instead of
  at 03:00 when there is nobody to click Allow. Every AppleScript call has a deadline, so a prompt
  waiting where nobody can see it reports itself instead of hanging the daemon.
- **The outbox.** Nothing sends inline. A run enqueues and moves on, so a slow channel can never
  turn a completed booking into a failure. Retries back off and then stop; something that needs a
  person is parked rather than retried, because retrying achieves nothing until they act; and a
  send interrupted by a crash is marked uncertain rather than repeated, since messaging someone
  twice is worse than admitting nobody knows whether it arrived.
- **Quiet hours** for messages to people and for routine good news. A failure you asked to hear
  about breaks through, because being told at 09:00 that the 08:00 booking failed is being told
  too late to act.
- Identical messages to the same recipient within ten minutes are dropped, since the usual cause
  is a bug and the person on the other end does not want it twice.

## Unreleased, after M3

### Fixed

Cleared from the same review's major findings, before starting the next milestone.

- **`dry_run` was accepted and then ignored.** A run asked to rehearse would really book, really
  send, really delete. It is now enforced at the tool layer rather than by asking the model
  nicely: an irreversible action in a rehearsal is recorded as what would have happened and does
  not happen. Verified against a real booking site, where a dry run left the booking count at zero.
- **A task whose run died mid-action could not be recovered.** Blocking it was right, since nobody
  knew whether the action went through, but there was no way to say what you found short of
  editing the database, so the safe state was a permanently stuck one. `POST /v1/tasks/{id}/holds`
  takes `already_happened` or `did_not_happen` and resolves it. It needs the approve scope, because
  that answer decides whether a real booking gets made.
- **Occurrences missed during an outage were recorded only when the catch-up plan took none of
  them.** Under the default policy a five-occurrence outage ran one and silently lost four.
- **Nothing stopped two runs of the same task overlapping.** A task slower than its own interval
  accumulated agents, each acting without knowing what the others had done.
- **Nothing validated a schedule when it was saved,** so an unparseable cron expression or a
  window time of "quarter past banana" was accepted and only failed at the moment it mattered.
- **Occurrence ids were minute-resolution,** so every occurrence of a schedule firing more than
  once a minute collapsed into one and all but the first were discarded as duplicates.
- The README claimed a unique index prevented double-booking. It prevents a duplicate run; the
  fence is what prevents a duplicate action, and the two are now described separately.

## M2b

### Added

- **Browser sidecar.** Playwright runs in a Node process the Rust core owns and supervises. The
  model never speaks to it: it emits symbolic actions naming refs from a snapshot, and Rust checks
  each one before it becomes a message on the pipe.
- **Our own accessibility snapshot** rather than Playwright's, so ref stability is under our
  control and secure fields are marked at the point of capture. A password box renders as
  `textbox "Password" [ref=e2] [secure] value=[hidden]`, so its contents cannot reach the model
  even by accident.
- **The credential fill path.** The agent names a credential and a field; it never sees the value.
  The secret is read from the keychain at the moment of use and released only to the exact domain
  the credential is registered against, which is also what defeats a lookalike page.
- **Domain allowlist**, enforced in three places: stated to the model so it plans within it,
  checked authoritatively in Rust, and blocked again in the sidecar's request router. An empty
  allowlist permits nothing rather than everything, because a task that has not said where it may
  go has not been taught yet. A near-miss domain is reported as a probable lookalike rather than a
  generic block.
- **The redactor.** Every secret resolved during a run is registered along with its URL-encoded
  and base64 forms, and everything headed for the journal, the logs, or the model is scrubbed
  through it. Journal writes additionally assert the line is clean and refuse to record it if not.
- **Captcha detection**, surfaced to the agent the moment it appears so it fails honestly instead
  of burning turns on something it is forbidden to solve.
- **Per-site browser profiles** with a lock tied to the run, so a logged-in session survives
  between runs and a crashed run cannot lock a site out permanently. Stale Chromium singleton
  locks are cleared before launch.
- Masked screenshots: password fields are obscured before the pixels exist, not blurred after.

### Verified against a real site

A local site with a genuine login and booking flow was used end to end. The agent logged in with a
stored credential and booked the right slot, reporting the confirmation number the server actually
issued. The password appears nowhere afterwards: not in the database, the logs, the journal, or
any artifact. Told to visit a site outside its allowlist, the agent was refused and failed with an
explanation. Told to type a bank credential into a different site, the fill was refused and
nothing was entered.

### Fixed

- **The CLI could hang forever after it had already answered.** A keychain call that stalls behind
  a macOS authorization prompt runs on a blocking thread, and a timeout only abandons the future:
  tokio cannot cancel the thread, so the runtime waited on it indefinitely. `doctor` printed its
  full report and then never returned. It now exits explicitly, which is the right end of a
  diagnostic anyway. Verified: returns in under a second where it previously never returned.
- **Added `errandd token --new`.** If the keychain copy of the API token becomes unreadable, the
  hash in the database still authenticates requests but nobody can read the value, and the only
  way out was editing the database by hand. This mints a replacement and revokes the old one.

## M2a

### Added

- **Contained Claude executor.** Runs are carried out by the Claude CLI spawned headless, with
  its capabilities established by testing rather than assumption. Four facts drove the design:
  `--allowedTools` only auto-approves and removes nothing; `--disallowedTools "*"` empties the
  list but takes the MCP tools with it; `Glob` and `Grep` cannot be removed at all; and the tool
  list is not stable between invocations. So the boundary is the **working directory**, since the
  un-removable tools are filesystem readers that refuse to leave cwd without a permission headless
  mode denies, and every run gets an empty scratch directory of its own.
- **Fail-closed containment assertion.** The tool list reported at session start is checked before
  the model can act. Anything beyond our own MCP tools kills the process and fails the run. This
  was verified by removing the deny list and watching it catch all 30 tools, including `Bash`,
  `WebFetch`, `Skill`, `Workflow` and `CronCreate`. A breach is its own terminal failure code and
  auto-pauses the task, because retrying an unsafe spawn is worse than not running.
- **MCP tool server** at `/mcp/runs/{run_id}`, the agent's only reach into the world, with a bearer
  minted per run so a tool call cannot touch another run's data. Four tools for this milestone:
  `read_brief`, `journal`, `finish`, `fail`.
- **The three-question failure contract, enforced end to end.** `fail` refuses an explanation that
  does not say what was attempted, why it could not finish, and what the person can do next.
- **Live run journal** streaming to the SSE feed as the agent works, in its own plain language.
- Per-run cost and token accounting, and a per-run stderr log, since the CLI reports only
  "exited with code 1" and the real cause is always on stderr.
- `scripts/dev-install.sh`, which installs the runner from a stable signed copy rather than from
  `target/debug`.

### Fixed

- **`errandd doctor` reported "No problems found" while displaying failures.** Four of its checks
  printed a failure mark without counting it. A diagnostic that misreports its own result is worse
  than no diagnostic.
- **Never point the LaunchAgent at `target/debug`.** cargo rewrites that file on every build, and a
  launchd-spawned process whose binary changed underneath it deadlocks in dyld while amfid
  validates the signature: alive, never reaching main, nothing listening, no log. Development
  installs now go through a stable signed copy, which is also the shape the shipping build uses.

## M1

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
