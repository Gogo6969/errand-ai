# @errand-ai/client

Talk to the [Errand-AI](https://github.com/Gogo6969/errand-ai) daemon running on your own machine.

```bash
npm install @errand-ai/client
```

```ts
import { ErrandClient } from "@errand-ai/client";

const errand = new ErrandClient({ token: process.env.ERRAND_TOKEN! });

// "book my court" in a chat window
const [task] = await errand.findTasks("court");
const run = await errand.runTask(task.id, { idempotencyKey: chatMessageId });

for await (const ev of errand.watchRun(run.id)) {
  if (ev.event === "step.finished") post(ev.data.title);
  if (ev.event === "run.finished") post(ev.data.summary);
  if (ev.event === "run.failed") post(ev.data.failure_human);
}
```

## Two things worth getting right

**Pass an idempotency key** whenever a user gesture might be retried. Without one, a dropped
connection followed by a retry is two bookings. With one, the retry returns the same run.

**Use `127.0.0.1`, not `localhost`.** The daemon listens on IPv4 loopback, and on some systems
`localhost` resolves to IPv6 first and simply fails to connect.

## Getting a token

Mint one scoped to what your program actually needs:

```bash
curl -H "Authorization: Bearer $(errandd token)" \
     -H 'Content-Type: application/json' \
     -d '{"name":"kinai","scopes":"read,run,webhook"}' \
     http://127.0.0.1:4477/v1/tokens
```

`read,run,webhook` lets a client list tasks, start them, and subscribe to its own outcomes. It
deliberately cannot rewrite a playbook, read a credential, or answer an approval gate, so a
compromised client cannot approve the booking it started.

## Webhooks

For a client that restarts, subscribe instead of holding a stream open:

```ts
const { secret } = await errand.subscribe("http://127.0.0.1:3000/errand-hook");
```

Every delivery carries `X-Errand-Signature`. Check it, and reject anything older than a few
minutes so a delivery cannot be replayed at you:

```ts
import { verifySignature } from "@errand-ai/client";

const ok = await verifySignature(
  secret,
  req.headers["x-errand-timestamp"],
  rawBody,
  req.headers["x-errand-signature"],
);
```

Errand only calls addresses on your own machine or network. A public URL is refused, because a
callback address a client supplies and Errand then fetches would otherwise be a way to make your
computer request whatever somebody chose.
