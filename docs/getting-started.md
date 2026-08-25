# Getting started

## What you need

- **macOS 12 or later.** Windows and Linux are later work.
- **The Claude command line tool**, signed in. This is what actually carries out
  a task. Install it and run `claude /login` once.
- **A Chrome-family browser.** Errand drives one you already have rather than
  downloading a 300 MB one of its own. It uses a separate profile, so your
  windows, tabs, history and saved logins are untouched.

Everything else — which AI answers the smaller questions, whether anything is
sent to a cloud at all, how you hear about results — is a choice you make in the
app.

## First run

Open Errand-AI. If something is not set up, the app says which thing and what to
do about it. From a terminal you can get the same answer in one line:

```bash
errandd doctor
```

It checks everything that has to be true for a task to run and prints the fix
for anything that is not.

## Teaching it one task

Start with something small and reversible. A booking is a poor first task; a
research or triage job is a good one.

**1. Describe it.** Write the job the way you would explain it to a person, and
say what to do when the obvious path is not there. This text is what the agent
actually reads, so vagueness costs you a run.

**2. Say which sites it may open.** A task with no sites cannot browse at all.
Type the plain form — `example.com`, not `www.example.com` — because subdomains
are included but it only works downwards.

**3. Teach it once.** Press **Teach it once** and watch. The agent works only
from your description this time. You can follow every step as it happens.

**4. Read the plan.** At the end it writes down what worked: each step as an
*intent* separately from the *hint* it used. Read it. Nothing runs on a schedule
until you approve it — that is the line between "it tried something once" and
"it does this alone while you are asleep".

**5. Give it a schedule.** Pick when it should run. Underneath, Errand shows
what the schedule really means and the next few times it will fire. If that
sentence is not what you meant, change the form until it is.

## Before you trust it with something irreversible

**Rehearse it.** A rehearsal runs the task normally but records anything
irreversible instead of doing it. Nothing is booked, sent or deleted. It is the
cheapest way to find out that a site has changed.

**Save the login properly.** Add it in Settings, bound to one site. Errand can
use it and cannot show it to you, and it will be refused on any other site — so
a convincing copy of that site gets nothing.

**Set a message limit** if the task will contact people, and add those people
deliberately, one task at a time.

## Where to next

- [Writing a task](tasks.md) — descriptions, sites, schedules, limits
- [Which AI does what](ai.md) — and how to keep it all on your own machine
- [Messaging people](messaging.md) — Telegram, and telling other people
- [When something is wrong](troubleshooting.md)
