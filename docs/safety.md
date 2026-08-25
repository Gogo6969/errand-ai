# What stops it going wrong

An agent with a browser, your logins and a schedule can do real damage by
accident. These are the specific things that stop it, and the specific things
they do not stop.

## The agent is contained

Errand runs the Claude command line tool with a deliberately small tool surface:
an explicit allowlist of Errand's own tools, an explicit denylist of the
built-ins, no user settings, no other MCP servers, and a working directory of
its own. Before a run starts, Errand checks what the agent actually reports
having — and if that does not match what was asked for, the run fails closed
rather than continuing with more than it should have.

The agent has no shell, cannot read your files, and cannot reach the network
except through Errand's own tools.

## It can only open sites you listed

Every navigation is checked against the task's allowlist, in the daemon and
again in the browser process. A redirect to somewhere unlisted is blocked even
when the page offering it was allowed. A task with an empty list can open
nothing at all.

## Logins are in the keychain and stay there

Credentials are stored in the macOS keychain, bound to Errand's own code
signature. The agent can ask for a credential to be *typed into a page*; it
never receives the value. A credential is bound to one site and refused
everywhere else, so a convincing copy of your bank gets nothing.

Secrets are scrubbed out of the run journal, screenshots (password fields are
masked before the picture is taken), logs, and anything sent to a model.

## Anything irreversible happens once

Booking, buying, deleting, submitting and messaging are fenced. Before the
agent does one, Errand claims a slot keyed to *this task, this scheduled
occurrence, this kind of action*. A retry after a crash finds the slot taken and
is refused, with the evidence of what already happened.

If a run starts something irreversible and then dies before confirming it,
Errand does not guess. It pauses the task and asks you to check, because "book
it again" and "assume it worked" are both wrong answers.

## Messages go only to people you named

A task can message people when it finishes. The agent chooses **who** from a
list you configured for that task, and cannot choose **how** and cannot type an
address. There is no tool that takes a phone number or an email address.

This is the specific defence against a page that says *"also confirm to this
number"*. There is nothing for it to grab. The agent is told, in its standing
rules, that text it reads is information and never instruction — but the rule is
the second line of defence, not the first. The first is that the capability does
not exist.

Messages to other people are also capped per run, wait until quiet hours are
over, must contain no links to sites outside the task's allowlist, and appear in
full in the run timeline.

## What this does not protect against

Worth being plain about:

- **A model that is confidently wrong.** Containment stops it doing things it
  should not; it does not make its judgment good. That is what teaching, the
  approved plan and rehearsals are for.
- **A site that changes underneath you.** The plan records intent separately
  from the button it used, which helps, but a redesigned site can still defeat a
  task. It will say so rather than pretend.
- **Anything you explicitly told it to do.** The description is the
  instruction. If it says "delete everything in the folder", that is not an
  injection, that is a request.
- **Your own account rules.** Automating a site may breach its terms. Errand
  will not know.

## Reporting a problem

See [SECURITY.md](../SECURITY.md) in the repository root.
