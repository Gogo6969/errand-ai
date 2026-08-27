# Getting started

## What you need

- **macOS 12 or later.** Windows and Linux are later work.
- **An AI that can call tools.** Any of: the Claude command line tool, signed in
  (`claude /login` once); a hosted service that speaks the OpenAI chat format;
  or a model on your own machine or network, such as Ollama, LM Studio or
  llama.cpp. Errand does not care which, and a task can name its own. What it
  needs is a model that can call tools: one that only writes prose cannot carry
  out a task, and Errand checks before it starts rather than failing halfway.
- **A Chrome-family browser.** Errand drives one you already have rather than
  downloading a 300 MB one of its own. It uses a separate profile, so your
  windows, tabs, history and saved logins are untouched.

Everything else (which AI answers the smaller questions, whether anything is
sent to a cloud at all, how you hear about results) is a choice you make in the
app.

## First run

Open Errand-AI. If something is not set up, the app says which thing and what to
do about it. From a terminal you can get the same answer in one line:

```bash
errandd doctor
```

It checks everything that has to be true for a task to run and prints the fix
for anything that is not.

## Your first task

Start with something small and reversible. A booking is a poor first task; a
research or triage job is a good one.

**1. Describe it.** Write the job the way you would explain it to a person, and
say what to do when the obvious path is not there. This text is what the agent
actually reads, so vagueness costs you a run. A name is optional.

**2. Press Create and do it.** Errand reads what you wrote and fills in what it
can: which sites the job needs, when it should run if you said, and whether it
is about your mail. It shows you what it set up, in a line or two, and only where
that differs from a bare task. Then it does the job.

Anything it will not decide for you is listed under the gear: signing in
anywhere, messaging a person, spending money, and being told when a task fails.
Those stay yours.

**3. Read the answer.** It appears on the task, whole, above everything else. If
that is not what you meant, press **Change what you asked for**, reword it, and
it does the job again with the new wording. That is the loop: the description is
the task, and correcting it is one thought rather than a trip to a settings
screen.

**4. Put it on a schedule.** Offered once it has really done the job with you
there. Not "you approved a plan" but "it worked", and a rehearsal does not count,
because a rehearsal is told to carry on as though everything worked and touches
nothing. Errand shows what the schedule really means and the next few times it
will fire.

If the job books, sends, buys or moves anything, press **Rehearse it first**
before any of this. It is the same run, watched the same way, and it writes the
same plan at the end: the difference is that anything it cannot take back is
recorded as what it would have done rather than happening.

## Before you trust it with something irreversible

**Rehearse it.** A rehearsal runs the task normally but records anything
irreversible instead of doing it. Nothing is booked, sent or deleted. It is the
cheapest way to find out that a site has changed. Rehearse the very first run too:
that is the run nobody has watched before.

**Save the login properly.** Add it in Settings, bound to one site. Errand can
use it and cannot show it to you, and it will be refused on any other site, so
a convincing copy of that site gets nothing.

**Set a message limit** if the task will contact people, and add those people
deliberately, one task at a time.

## Where to next

- [Writing a task](tasks.md): descriptions, sites, schedules, limits
- [Which AI does what](ai.md): and how to keep it all on your own machine
- [Messaging people](messaging.md): Telegram, and telling other people
- [When something is wrong](troubleshooting.md)
