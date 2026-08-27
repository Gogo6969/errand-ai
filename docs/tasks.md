# Writing a task

A task is four things: what you want done, which sites it may use, when it
should happen, and what it is allowed to spend doing it.

## The description is the instruction

The description is not a label. It is the text the agent actually reads, and it
is the source of truth even after a plan has been written. Write it the way you
would explain the job to a competent person who has never done it:

> Go to the club website, sign in, and book the Wednesday 19:00 court on the
> outdoor side if there is one. If Wednesday is full, take Thursday at the same
> time. Tell me the confirmation number.

Two things earn their place in a description:

- **What to do when the obvious path is not there.** A page moves, a slot is
  gone, a form asks for something new. Say what you would want done.
- **What not to do.** "Never pay for anything", "do not book more than one".
  These become part of the plan and survive into every future run.

Vagueness costs you a run. "Check my email" gives the agent nothing to aim at;
"open the inbox and tell me which unread messages are from a person rather than
a company" does.

## Sites it may open

A task can only open the sites you list, and **a task with no sites cannot
browse at all**. Errand tidies what you type into the exact form it compares
against, so pasting a whole URL is fine.

- **Subdomains are included.** `example.com` covers `www.example.com` and
  `booking.example.com`.
- **It only works downwards.** `www.example.com` does *not* cover
  `example.com`, and most sites bounce between the two, so list the plain form.
- **There are no wildcards.** `*.example.com` looks reasonable and would match
  nothing, so it is refused rather than saved.
- **A bare ending is refused.** `co.uk` would hand the task every site in the
  country.
- **Redirects are checked too.** A link that leaves the allowlist is blocked
  even if the page that offered it was allowed. This is what stops a page that
  looks like your bank from being treated as your bank.

Order matters. The first site decides which browser profile the task uses, and
the profile is where its saved logins live. Reordering the list can log a task
out.

## When it runs

Pick from the everyday shapes (every day, every week, every month, once at a
set time) and Errand writes the schedule for you. Underneath the form it shows
what the engine will *really* do and the next few times it will happen. If the
sentence disagrees with what you meant, believe the sentence: it comes from the
part that does the running.

Three settings are worth understanding:

**Time zone.** Runs follow local time, so eight in the morning stays eight in
the morning across a clock change rather than drifting by an hour.

**Missed runs.** If your Mac was asleep:
- *Run it once, late*: the sane default.
- *Skip it*: right for anything time-sensitive. Nobody wants a court booked
  for a slot that has already passed.
- *Make up every missed run*: only for tasks where each run does distinct work.

**Spreading the start.** If a booking opens at a fixed moment, every bot in the
country arrives on the same second. Starting a minute either side helps, and
makes Errand look less like a machine.

Changing a schedule never replays the past. Errand will not go back and run the
slots your new schedule would have had earlier today; the first run under a new
schedule is the next one.

## Limits

Every run has a ceiling on steps, minutes, money and messages. A run that goes
round in circles is stopped rather than left going, and when it stops it says
which ceiling it hit, so you can tell a genuinely slow website from an agent
that is stuck.

The message limit is the one worth setting deliberately: it is the maximum
number of messages a single run may send to other people, and the report sent
when the task finishes counts against it too. A task linked to more people than
its limit allows will tell the first few and say in the timeline who it did not
tell.

## Which AI does it

Every task follows the model chosen on the AI screen unless it names one of its
own. **Which AI does this task** on the task's page holds that choice, with
*Default* first, naming what the default currently is.

It is worth setting where the work is private. Whichever model carries the task
out is the model that reads what the task reads, so a task that goes through
your mail can be kept on a model running on your own machine while everything
else uses a service.

A model Errand has asked and found unable to use tools cannot be picked, because
it cannot drive a browser. One that is merely switched off can: if it is not
available when the task runs, the run goes ahead on the next model that works
and says in its timeline what it was asked for and why it could not have it.

## Teaching it

The first run works only from your description, with you watching. At the end
the agent writes a plan: each step recorded as an *intent* (what it was trying
to achieve) separately from the *hint* (the button it used last time). Sites
move their buttons; intentions do not.

You read that plan and approve it. **Nothing runs on a schedule until a plan is
approved**, and that gate applies whether you arm the task or change it onto a
schedule afterwards.

There are two ways to teach it, and the difference is whether the job really
happens:

- **Teach it once, for real.** It books the court, sends the message, moves the
  post. Right for a job where nothing is lost by doing it.
- **Teach it as a rehearsal.** The same run, watched the same way, writing the
  same plan, with anything irreversible recorded as what it would have done.
  Nothing is booked, sent, bought or moved.

**If the job books, sends, buys or moves anything, rehearse it first.** A task
cannot run at all until it has been taught, so the teaching run is the first
run of the job and the only one you watch from the beginning.

A plan written by a rehearsal says so, above the Approve button, so you always
know whether the run behind a plan really did what it describes.

## Rehearsing

*Rehearse* runs the task normally but records anything irreversible instead of
doing it. Nothing is booked, sent or deleted. It is the cheapest way to find out
that a site has changed.

Rehearsing is available at both ends: on the first run, as a way of teaching it
(above), and on any later run of a task that already has an approved plan.
