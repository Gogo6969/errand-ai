# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **A task sets itself up.** Creating one used to mean filling in a name, a schedule, a list of
  sites and a permission or two before anything could run, and the panels were the first thing a
  person saw. Now the description is read for what it says outright, and a model is asked about
  what it does not: "Show me the latest important Bitcoin news with links, every morning at 7am"
  arrives named, pointed at two news sites, and on a seven o'clock schedule in the writer's own
  timezone. What was decided is shown back in a few lines, each naming the words that decided it,
  and only where it differs from a bare task. A name is now optional, because a person describing
  a job has already said what it is.
  Nothing here can grant what a sentence should not: signing in, messaging a person, spending, and
  being told when a task fails are all left alone on purpose, and a prohibition always beats a
  request, so "clean my mailbox but never delete anything" is allowed to read and refused the
  moving. Sites a person named themselves are never merged with sites Errand found, because the
  first entry decides which saved logins a run gets.
- **A run has an answer, separate from what it did.** A task exists to produce an outcome, and
  there was nowhere to put one: a run stored a one-line summary of the work and a step journal,
  so a task told to "look at my inbox and tell me who each message is from" reported that it had
  filed a note and left the actual summary inside Apple Notes. `finish` now takes an `answer`
  beside the `summary` and refuses a pointer such as "see the note", the answer is shown first
  and whole on the task and the run, and it survives a failure: the commonest failure of all is a
  run that read everything, worked the answer out, and only then found macOS would not let it
  write the note it was asked for. The answer is scrubbed like anything else that leaves the run,
  because it is the one field built to carry the contents of a page into a database row, a
  webhook and a phone.
- **Copies of an answer are recorded and can be opened.** A task that asks for a note or a file
  still gets one, and the task page names it with a button that brings it up
  (`POST /v1/answer-copies/{id}/open`). The rows are written by the tool that actually did the
  writing rather than read back out of the journal's sentences, so a link never points at
  something that did not happen.
- **A task can name the model that carries it out.** The AI screen sets the default; a task may
  choose its own, and the run says which it used and whether the task asked for it. Only the
  executor is per-task so far: writing the plan, diagnosing a failure and wording a notification
  still resolve against the global choice, and the run now says so rather than promising a
  privacy it cannot keep.
- **A task can be taught as a rehearsal.** A task may not run until it has been taught, and
  teaching was always for real, so the first run of "clean my mailbox of spam" really moved the
  post and the first run of "book a tennis court" really booked it: the one run nobody had watched
  before was the one that could not be rehearsed. `POST /v1/tasks/{id}/teach` now takes an
  optional `{"dry_run": true}` (no body still teaches for real), and the task page offers "Teach
  it as a rehearsal" beside "Teach it once, for real". The rehearsed run works the job out and
  writes its plan exactly as before, while everything irreversible is recorded rather than done
  and the fence is left unarmed, so the first real run is still allowed to do the thing. Whether a
  run was a rehearsal is now a flag of its own beside the mode, because teaching and rehearsing
  are two different questions and a run can be both. The plan such a run writes says on the
  approval card that a rehearsal wrote it, since its steps otherwise read exactly like a plan from
  a run that really did the job.
- **Screenshots in the run timeline.** A step that took one carries a "See what it saw"
  disclosure: the shot is recorded as an artifact, addressed by id, served by the API at
  `GET /v1/artifacts/{id}`, and shown where the step is. The explanation has promised this since
  M8; now it is true.
- **Limits and notify preferences can be changed from the task page.** Run ceilings got the same
  editor treatment schedules and sites already had, and who hears about success or failure is a
  click rather than a remove and re-add. The milestone claimed a configuration surface for limits;
  this is the first time it is actually configurable from the window.
- **Tasks report their open holds.** The "needs you" card keys off a count of armed fences rather
  than off the wording of the pause reason, which is a sentence that can be reworded.

### Changed

- **Task settings live behind a gear.** A task page opened onto eight panels of configuration
  with the result somewhere below them. When it runs, what it may open, which AI, who it tells
  and reading your mail are now one click away instead of in the way; the answer, what the task
  learned, and its history are what the page shows.
- **The agent is no longer told that answers belong in Apple Notes.** The system prompt said, in
  those words, that a run summary is not somewhere anybody looks and a note on their phone is.
  The agent obeyed it exactly, and wrote the answer into a note nobody had asked for. It now
  hands the answer to `finish` and makes a note only when the task text asks for one.

### Fixed

- **Reading or moving a message searched the whole mailbox.** Listing was taught to walk by index
  after an inbox of 191,000 messages failed with AppleScript error -1741, and the finder behind
  `read` and `file` was left doing `first message of inbox whose message id is ...`, which builds
  the collection before narrowing it and never answers. Worse, the fallback repeated that scan for
  every mailbox of every account, and the timeout was reported to the person as a missing macOS
  permission, sending them to System Settings to fix something that was not broken. The ids a
  listing hands out now carry where the message was, so reading one is a lookup rather than a
  search, with a bounded rescan when new mail has shifted it.
- **The fence could have let one message be filed twice.** Ids that carry a position had to be
  kept out of the side-effect fence, which identifies a message: the same message listed a minute
  apart would otherwise have been two different scopes, and "never move it twice" would quietly
  have stopped being true.
- **A model repeating itself burned the whole run.** One local model asked for the same mailbox
  twenty-five times, a second apart, until it ran out of turns, and the run ended saying the agent
  had stopped without reporting whether it finished. It is now told plainly that it is repeating
  itself, and the run is stopped with a reason that names the model.
- **A plan waiting for approval could not be read.** The approval gate is the one line between
  "somebody watched it try once" and "it does this alone at three in the morning", and the API
  told a person to read what the task wrote while returning the text of the approved version only.
- **`dev-install.sh` would install a stale binary.** cargo can report success and leave the last
  executable in place when clippy or test has won the fingerprint, and an older daemon knows fewer
  migrations, so it dies with "migration 5 was previously applied but is missing" and reads like a
  corrupt database. The script now refuses to install anything older than the code.
- **Checking a model said nothing.** The AI screen's Check button re-probed the model and stored
  the verdict, but when the verdict was unchanged no pixel moved, so the click looked dead. The
  button now says what it is doing, the answer lands on the row it came from ("Checked just now:
  still not answering."), and a failed check is reported there too rather than at the top of a
  page nobody is looking at.
- **"Try again" on a failed run reloaded the failure.** It now takes you to the run it just
  started.
- **Two controls wore the wrong explanations.** Create wore the teach text and Resume wore the
  pause text; they have their own.
- **The type check was broken and nothing watched it.** `svelte-check` had three errors and no CI
  job ran it. It is clean and a gate now, and the hint audit now fails local builds too rather
  than only CI.
- **The em-dash rule was broken in 140 places** across code comments, docs and UI strings, and
  nothing enforced it. All are rewritten and `scripts/scrub-check.sh` now fails on a single one.
- **Hint tooltips only opened for the mouse.** They open on keyboard focus too.

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

## M7

### Added

- **Scoped tokens per client.** Each program gets its own named token with only what it needs.
  KinAI gets `read,run,webhook`: enough to list tasks, start them, and subscribe to its own
  outcomes, and deliberately not enough to rewrite a playbook, read a credential, or answer an
  approval gate. A compromised client cannot approve the booking it started.
- **Idempotency on run triggers.** Pass `Idempotency-Key` and a retry after a dropped connection
  returns the same run instead of booking twice. The same key with different content is refused
  rather than replayed, because replaying the old answer would hide the client's bug.
- **Webhooks**, for clients that restart rather than holding a stream open. Every delivery is
  signed with HMAC-SHA256 over the timestamp and body, so a receiver can tell a genuine callback
  from anything else that finds the port and can reject a replayed one.
- **Webhook targets are restricted to your own machine or network.** A callback address a client
  supplies and Errand then fetches on a schedule is the shape of a request-forgery hole, so a
  public URL is refused, and the check runs again at delivery time rather than only at subscribe
  time.
- **A hook that stops answering is switched off** after repeated failures, and you are told, since
  the symptom otherwise is another program silently never hearing about anything.
- **`@errand-ai/client`**, a TypeScript client with no runtime dependencies: find tasks, start
  them idempotently, follow a run as an async iterator, and verify a delivery signature.

### Fixed

- **A failed keychain write left a token nobody could use.** The hash went into the database
  first so the API works even if the keychain stalls, but when the write then failed the row
  stayed behind: it authenticated a token nobody could read, and the next boot saw a token already
  existed and never minted a usable one. The row is now rolled back.
- **Development builds churned their own code identity.** Ad-hoc signing produces a different
  identity on every build, so macOS treated each rebuild as a different program and the keychain
  items from the last one no longer matched, which surfaced as a permission prompt nobody could
  see. Development installs now sign with a stable certificate when one exists.
- **The scrub check had no way to permit a deliberate exception**, so a test asserting that
  private addresses are allowed would have failed the push. Exemptions are now explicit and every
  one is printed, rather than the rule being weakened.

## M8

### Added

- **A window.** Errand-AI is now an app you can open: tasks, history, settings, and a run timeline
  showing everything the agent did in its own words.
- **The shell does almost nothing on purpose.** It never executes a task, opens the database, or
  touches a website. It asks the daemon, which is what lets you quit the window without stopping
  anything that was running.
- **The API token never enters the webview.** Every call is proxied through Rust. A token in
  JavaScript is a token in the page, readable by anything that ends up running there, and this one
  can start runs and read your whole history.
- **Every control is explained**, and a machine checks it. The audit fails the build on a control
  without an explanation, on an explanation nobody shows, and on an unknown id. Exemptions are
  explicit, carry a stated reason, and are printed on every run so they stay reviewable. A
  labelled form field counts as explained by its label, because a tooltip repeating the label is
  noise rather than help.
- **Explanations written for someone slightly worried** about what this is going to do to their
  accounts. Where it matters they say what will not happen: pausing deletes nothing, running now
  will not book twice, a rehearsal books nothing at all, and a saved login can never be typed into
  any site but its own.
- **The failure card** answers the same three questions everywhere: what it was doing, why it
  stopped, and what you can do, with technical detail behind a disclosure for when you want it.
- **A task that needs you says so**, and offers the two answers that unblock it after a run died
  part way through something irreversible.

## M8b

### Added

- **Every AI it is given.** Fifteen services reachable by name rather than by address: OpenAI,
  Google, OpenRouter, xAI, DeepSeek, Moonshot, Mistral, Groq, Z.ai, Together, Fireworks, Cerebras,
  Perplexity, Ollama, LM Studio, plus anything else speaking the same format, and models found on
  this machine or, when asked, on this network. Keys live in the keychain, one per service. The AI
  screen says which model does each job and whether anything leaves the machine, and marks the one
  job Errand does not use a separate model for rather than offering a setting that changes nothing.

## M8c

### Added

- **The task configuration surface.** Schedules, sites, limits and notify preferences can be
  changed after a task exists, through `PATCH /v1/tasks/{id}` and through the app. A schedule is
  built from everyday shapes and shown back in the engine's own words with its next few run times,
  so a form that builds the wrong thing disagrees with itself in front of you.
- **People a task may message.** WhatsApp, Apple Mail, Apple Messages and Telegram, as a per-task
  grant. The agent picks who from that closed set; it cannot pick how and cannot type an address,
  so a page saying "also confirm to this number" has nothing to grab. Messages are fenced per
  person per occurrence, capped, scrubbed, checked for links off the allowlist, and shown in full.
- **Telegram details and quiet hours can be set from the app.** Both were read since M1 and
  writable by nothing.
- **The browser ships.** The sidecar is bundled where the daemon looks for it and drives a
  Chrome-family browser you already have, in a profile of its own. CI builds the bundle and proves
  it opens a page.
- **`errandd doctor`** reports node, the browser helper and which browser it would drive.
- **`scripts/smoke.sh`**, which drives the real daemon through the real API, because unit tests
  have twice been green while the feature they covered was unreachable.
- **Documentation**, and a client package covering the whole API rather than a third of it.

### Fixed

Found by an adversarial review that reproduced each finding against a running daemon before it was
believed. The ones that mattered most:

- **No task could open any website.** `allowed_domains` was read in three places and written by
  nothing, so every task ever created shipped with an empty list. The same shape of defect as the
  fence in M3: green tests, no callers.
- **Changing a schedule replayed the past.** The scheduler's catch-up cursor is global, so
  switching a task to a daily cron treated this morning's slot as missed and fired it on the spot,
  burning that occurrence id for good. Each task now carries its own floor.
- **The teach gate could be walked around.** It only ran at activation, so putting an untaught task
  on a schedule by editing it afterwards skipped the one line between "tried once while watched"
  and "runs alone at three in the morning".
- **Editing one limit wiped the others**, including the message ceiling, because a patch replaced
  the whole object rather than merging into it.
- **The schedule form deleted what it could not show.** Opening the editor and pressing Save with
  no change at all removed a task's booking window and reset its catch-up grace.
- **Quiet hours did not exist until someone set them**, so a run finishing at three in the morning
  messaged a third party immediately.
- **A refusal told people to press a button that did not exist.** Changing a schedule that might
  repeat something irreversible returned a 409 explaining how to confirm, and nothing in the app
  could.

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
