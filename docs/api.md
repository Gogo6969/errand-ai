# The API

Errand's own window is a client of this API, which is the only reliable way to
keep an integration API honest: if a screen can do it, so can you.

```
http://127.0.0.1:4477
```

Loopback only. Use the numeric address rather than `localhost`: the listener is
IPv4, and `localhost` resolves to IPv6 first on some systems, which fails in a
way that looks like the daemon being down.

## Authenticating

Every request but `/v1/health` needs a bearer token.

```bash
curl -H "Authorization: Bearer $(errandd token)" http://127.0.0.1:4477/v1/tasks
```

Mint one for another program with `POST /v1/tokens`, and give it only the scopes
it needs. Errand stores a hash, so a token is shown once and never again.

| Scope | Lets the holder |
|---|---|
| `read` | See tasks, runs and settings |
| `run` | Start a run |
| `webhook` | Subscribe to events |
| `approve` | Approve a plan, resolve a hold, let a task message a person |
| `manage` | Create and change tasks, credentials, channels, models |
| `admin` | Everything, including tokens |

`approve` is separate from `manage` on purpose. Approving a plan and letting a
task message someone both decide whether a real, irreversible thing happens.

Errors come back as `{ "code": "...", "title": "...", "detail": "..." }`, and
`detail` is written for a person to read. Show it.

## From TypeScript

```bash
npm install @errand-ai/client
```

```ts
import { Errand } from "@errand-ai/client";

const errand = new Errand({ token: process.env.ERRAND_TOKEN! });

const [task] = await errand.listTasks();
await errand.updateTask(task.id, { allowed_domains: ["club.example"] });
const run = await errand.runTask(task.id);

for await (const event of errand.watchRun(run.id)) {
  console.log(event.event, event.data);
}
```

## Tasks

| | |
|---|---|
| `GET /v1/tasks` | List. `?include_archived=true` for the rest. |
| `POST /v1/tasks` | Create. `name`, `description`, and optionally `emoji`, `schedule`, `allowed_domains`, `notify`, `limits`. |
| `GET /v1/tasks/{id}` | One task, including `schedule_describes` and `schedule_preview`. |

Every task response also carries `open_holds`: how many armed irreversible
actions are waiting on a person to say what happened. Above zero, the task is
blocked until someone resolves the hold, and `POST /v1/tasks/{id}/holds` is the
way to do it.
| `PATCH /v1/tasks/{id}` | Change any of the above. Absent fields are left alone. || `POST /v1/tasks/{id}/teach` | Run it once in teach mode, so it can write a plan. No body teaches it for real; `{"dry_run": true}` teaches it as a rehearsal, which still writes a plan but records anything irreversible instead of doing it. |
| `POST /v1/tasks/{id}/run` | Run now. `{"dry_run": true}` rehearses without doing anything. |
| `POST /v1/tasks/{id}/activate` | Arm it, once a plan is approved. |
| `POST /v1/tasks/{id}/pause` · `/resume` | Stop and restart scheduled runs. |
| `GET /v1/tasks/{id}/playbook` | The plan, and any version waiting for approval. |
| `POST /v1/tasks/{id}/playbook/{version}/approve` | Approve one. |
| `POST /v1/tasks/{id}/holds` | Say what you found after an interrupted irreversible action. |

Two refusals from `PATCH` are worth handling rather than showing raw:

- **`task_not_taught` (409)**: the task has no approved plan, so it cannot be
  put on a schedule. This is the one line between "tried once while watched" and
  "runs alone at three in the morning", and it applies to `PATCH` as well as
  `activate`.
- **`schedule_change_may_repeat` (409)**: the new schedule's first run comes
  sooner than the old one's would have, and something irreversible has already
  been done for this slot. Retry with `"acknowledge_repeat": true` only after a
  person has read that.

Changing a schedule never replays the past: each task carries a floor, so the
first run under a new schedule is the next one, not a missed one.

### Sites

`allowed_domains` is a list of bare hosts. A full URL is accepted and tidied.
Subdomains are included; wildcards, single labels and bare public suffixes are
refused with a message saying what to type instead, because they would save
happily and match nothing. The response may carry `warnings`: things worth
saying but not worth refusing over, such as `www.x.com` without `x.com`.

Order matters: the first entry decides which browser profile the task uses, and
that profile holds its saved logins.

A teach run adds to this list on its own. Most sites serve their code from a
second address nobody thinks to type (x.com's is abs.twimg.com), and a task
allowed only the address a person names gets a page that cannot start. So a
teach run, where somebody is present, writes the addresses a page could not
load without into the task, and says which in the run's timeline. Only ones a
page was refused several scripts from: a single script is a sign-in button or
an analytics tag, not the site. An ordinary run never widens the list. It says
which address was refused and leaves the decision to a person.

### Which model carries a task out

`model_id` names one of the models from `GET /v1/ai`, by id. Null means the task
has not chosen, and it follows the executor binding on the AI screen. It is
worth setting where the work is private: whichever model carries the task out is
the one that reads what the task reads, so a task that opens a mailbox can be
kept on a model of your own while everything else uses a service.

On `PATCH`, leaving `model_id` out and sending it as `null` mean different
things. Out means unchanged, so an edit about anything else keeps the model the
task was given. `null` (or an empty string) puts it back on the default.

A model that Errand has asked and found unable to use tools is refused here, the
same as it is for the executor binding. One that is merely switched off, removed
later, or put out of reach by *keep everything on this machine* is not: the run
falls through to the next usable model rather than failing, and says in its
journal what it was asked for and why it could not have it.

### Schedules

```json
{ "kind": "cron", "expr": "0 0 8 * * *", "tz": "Europe/Vienna",
  "catch_up": "run_once_late", "jitter_s": 60 }
```

`kind` is `manual`, `once` (with `at`) or `cron` (with `expr`). **Six fields,
seconds first**: an expression copied from elsewhere usually has five and will
mean something quite different.

`POST /v1/schedule/preview` takes a schedule and returns
`{valid, describes, preview, problem}`: the engine's own words for what it
means, and the next few times it would really fire. Call it before saving one.

## Runs

| | |
|---|---|
| `GET /v1/runs` | `?task_id=` and `?limit=`. |
| `GET /v1/runs/{id}` | One run, with every step. |
| `GET /v1/runs/{id}/stream` | Server-sent events, live. |
| `GET /v1/events` | Every run, live. |
| `GET /v1/artifacts/{id}` | A file a run left behind, such as a screenshot. Steps that have one carry an `artifact_id`. Addressed by id, never by path. |

Every run carries `mode` (`normal`, `teach` or `dry_run`) and `rehearsal`. They
are two different questions, and a teach run can be a rehearsal too, so **read
`rehearsal` and not the mode** when what you want to know is whether anything
the run did was real.

## People a task may message

| | |
|---|---|
| `GET`/`POST /v1/recipients` | The address book. `label`, `channel`, `address`. |
| `DELETE /v1/recipients/{id}` | Forget someone. |
| `GET /v1/tasks/{id}/recipients` | Who this task may tell. |
| `POST /v1/tasks/{id}/recipients` | Let it. **Needs `approve`.** `{recipient_id, on_success, on_failure}` |
| `DELETE /v1/tasks/{id}/recipients/{recipient_id}` | Stop it. |

Saving a person does not let anything message them. The per-task link is the
grant, and it is what stops one task from telling someone about another task's
work.

## Credentials, channels, models

| | |
|---|---|
| `GET`/`POST /v1/credentials`, `PATCH`/`DELETE /v1/credentials/{id}` | Logins. Write-only: the secret never comes back. `PATCH` takes `label`, `username` and `secret`, each optional; a `secret` replaces the stored one and anything left out is left alone. The `domain` is not editable, because that binding is what keeps a login off a lookalike site. |
| `GET /v1/channels` | How each way of reaching you is doing. |
| `POST /v1/channels/{channel}/config` | `{secrets, settings}`. Secrets go to the keychain. |
| `POST /v1/channels/{channel}/test` · `/enable` | Send yourself one; ask macOS for permission. |
| `GET /v1/settings` | Settings the engine reads, such as `messaging.quiet`. |
| `GET /v1/ai` | Which model does each job, and whether anything leaves the machine. |
| `GET /v1/ai/catalogue` | Services Errand knows by name. |
| `POST /v1/ai/providers`, `DELETE /v1/ai/providers/{id}`, `POST /v1/ai/providers/{id}/test` | Add, remove, check. |
| `POST /v1/ai/discover?scan_network=true` | Look for models here, and optionally on your network. |
| `POST /v1/ai/roles/{role}` | Which provider does which job. |
| `POST /v1/ai/local-only` | Refuse to send anything off this machine. |

## Webhooks

`POST /v1/webhooks` with `{url, events}` returns a secret. Deliveries carry
`X-Errand-Signature` and `X-Errand-Timestamp`; verify both, and reject anything
more than a few minutes old so an old delivery cannot be replayed at you. The
client package exports `verifySignature` for this.

Targets must be loopback or a private address unless you allow otherwise:
Errand will not be turned into a way of reaching arbitrary hosts.
