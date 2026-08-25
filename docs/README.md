# Errand-AI documentation

Errand-AI runs real errands on a schedule. You describe a job the way you would
describe it to a person, it tries it once while you watch, writes down what
worked, and then does it on its own.

The app explains itself as you use it — every control has a tooltip, and that is
enforced by a build check, not by good intentions. These pages are for the
things a tooltip is too small to hold.

| | |
|---|---|
| [Getting started](getting-started.md) | Install it, teach it one task, watch it run. |
| [Writing a task](tasks.md) | Descriptions, sites, schedules, limits. |
| [Which AI does what](ai.md) | Claude, hosted services, models on your own machines. |
| [Messaging people](messaging.md) | Telegram, WhatsApp, Apple Mail and Messages. |
| [How it is built](architecture.md) | Two processes, and why. |
| [What stops it going wrong](safety.md) | Containment, the allowlist, the side-effect fence. |
| [The API](api.md) | For talking to Errand from another program. |
| [When something is wrong](troubleshooting.md) | `errandd doctor`, and the usual causes. |

## The short version

- **Nothing runs unattended until you have approved a written plan.** Teaching a
  task produces a plan in plain markdown. You read it. Nothing follows it until
  you say so.
- **A task can only open sites you listed.** Everything else is refused,
  including a redirect from a site that is on the list.
- **Logins live in your macOS keychain.** Errand can use them and cannot show
  them to you. They are never written to its database, its logs, its
  screenshots, or anything sent to an AI.
- **Anything irreversible happens at most once per scheduled slot.** A retry
  after a crash does not book twice.
- **When it cannot do something, it says so and says why.** It does not report
  a job as done when it is not.
