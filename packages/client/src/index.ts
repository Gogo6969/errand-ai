/**
 * Errand-AI client.
 *
 * Talks to the Errand daemon on your own machine. Nothing here reaches the
 * internet: the daemon listens on loopback, and the token is minted locally.
 *
 * Use 127.0.0.1 rather than localhost. The listener is IPv4, and on some
 * systems localhost resolves to IPv6 first and simply fails to connect.
 */

export type Scope = "read" | "run" | "webhook" | "approve" | "manage" | "admin";

export interface ErrandOptions {
  /** Where the daemon is. Defaults to the loopback address it listens on. */
  baseUrl?: string;
  /** A token minted with `errandd token`, or through the tokens endpoint. */
  token: string;
  /** Milliseconds before a request is given up on. */
  timeoutMs?: number;
}

export interface Task {
  id: string;
  name: string;
  emoji?: string | null;
  description: string;
  status: "draft" | "teaching" | "ready" | "paused" | "archived";
  schedule: unknown;
  playbook_version?: number | null;
  next_run_at?: string | null;
  paused_reason?: string | null;
  auto_paused: boolean;
}

export interface Failure {
  code: string;
  /** Written for a person. Safe to show a user as it stands. */
  plain_reason: string;
  technical?: string;
}

export interface Run {
  id: string;
  task_id: string;
  occurrence_id: string;
  mode: "normal" | "teach" | "dry_run";
  trigger: string;
  status:
    | "armed" | "queued" | "preflight" | "holding" | "running" | "healing"
    | "waiting_input" | "takeover" | "succeeded" | "failed" | "cancelled" | "skipped";
  summary?: string | null;
  failure?: Failure | null;
  cost_usd: number;
  started_at?: string | null;
  finished_at?: string | null;
}

export interface Step {
  seq: number;
  ts: string;
  kind: string;
  title: string;
  ok: boolean;
}

/** An error the daemon reported, carrying its machine-readable code. */
export class ErrandError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "ErrandError";
  }
}

export class ErrandClient {
  private readonly baseUrl: string;
  private readonly token: string;
  private readonly timeoutMs: number;

  constructor(opts: ErrandOptions) {
    this.baseUrl = (opts.baseUrl ?? "http://127.0.0.1:4477").replace(/\/$/, "");
    this.token = opts.token;
    this.timeoutMs = opts.timeoutMs ?? 15_000;
  }

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
    extraHeaders?: Record<string, string>,
  ): Promise<T> {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), this.timeoutMs);
    try {
      const res = await fetch(`${this.baseUrl}${path}`, {
        method,
        headers: {
          Authorization: `Bearer ${this.token}`,
          ...(body ? { "Content-Type": "application/json" } : {}),
          ...extraHeaders,
        },
        body: body ? JSON.stringify(body) : undefined,
        signal: ctrl.signal,
      });

      const text = await res.text();
      const json = text ? JSON.parse(text) : {};
      if (!res.ok) {
        throw new ErrandError(
          res.status,
          json.code ?? "unknown",
          json.detail ?? json.title ?? `Errand returned ${res.status}`,
        );
      }
      return json as T;
    } finally {
      clearTimeout(timer);
    }
  }

  /** Is the daemon up? Cheap enough to call before anything else. */
  async health(): Promise<{ status: string }> {
    return this.request("GET", "/v1/health");
  }

  async listTasks(): Promise<Task[]> {
    const r = await this.request<{ items: Task[] }>("GET", "/v1/tasks");
    return r.items;
  }

  /** Find tasks by a word in their name or description. */
  async findTasks(query: string): Promise<Task[]> {
    const q = query.toLowerCase();
    const all = await this.listTasks();
    return all.filter(
      (t) =>
        t.name.toLowerCase().includes(q) ||
        t.description.toLowerCase().includes(q),
    );
  }

  async getTask(id: string): Promise<Task> {
    return this.request("GET", `/v1/tasks/${id}`);
  }

  /**
   * Start a run.
   *
   * Always pass an idempotencyKey when a user gesture might be retried, such
   * as a chat message id. Without it, a dropped connection and a retry become
   * two bookings; with it, the retry returns the same run.
   */
  async runTask(
    id: string,
    opts: { dryRun?: boolean; idempotencyKey?: string } = {},
  ): Promise<Run> {
    return this.request(
      "POST",
      `/v1/tasks/${id}/run`,
      { dry_run: opts.dryRun ?? false },
      opts.idempotencyKey ? { "Idempotency-Key": opts.idempotencyKey } : undefined,
    );
  }

  async getRun(id: string): Promise<Run & { steps: Step[] }> {
    return this.request("GET", `/v1/runs/${id}`);
  }

  async listRuns(taskId?: string, limit = 20): Promise<Run[]> {
    const q = new URLSearchParams();
    if (taskId) q.set("task_id", taskId);
    q.set("limit", String(limit));
    const r = await this.request<{ items: Run[] }>("GET", `/v1/runs?${q}`);
    return r.items;
  }

  /**
   * Follow a run as it happens.
   *
   * Yields each event until the run ends. Written as an async iterator so a
   * caller can simply `for await` it and forward the titles into a chat.
   */
  async *watchRun(runId: string): AsyncGenerator<{ event: string; data: any }> {
    const res = await fetch(`${this.baseUrl}/v1/runs/${runId}/stream`, {
      headers: { Authorization: `Bearer ${this.token}` },
    });
    if (!res.ok || !res.body) {
      throw new ErrandError(res.status, "stream_failed", "Could not follow that run.");
    }

    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    let event = "";

    while (true) {
      const { done, value } = await reader.read();
      if (done) return;
      buffer += decoder.decode(value, { stream: true });

      let idx: number;
      while ((idx = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, idx).trimEnd();
        buffer = buffer.slice(idx + 1);
        if (line.startsWith("event:")) {
          event = line.slice(6).trim();
        } else if (line.startsWith("data:")) {
          const raw = line.slice(5).trim();
          try {
            const data = JSON.parse(raw);
            yield { event, data };
            if (event === "run.finished" || event === "run.failed") return;
          } catch {
            // A keep-alive comment or a partial frame; wait for more.
          }
        }
      }
    }
  }

  /** Ask to be called back when runs end, instead of holding a stream open. */
  async subscribe(
    url: string,
    events: string[] = ["run.finished", "run.failed"],
  ): Promise<{ id: string; secret: string }> {
    return this.request("POST", "/v1/webhooks", { url, events });
  }

  async unsubscribe(id: string): Promise<void> {
    await this.request("DELETE", `/v1/webhooks/${id}`);
  }
}

/**
 * Check that a delivery really came from Errand.
 *
 * Compare against the `X-Errand-Signature` header, and reject anything whose
 * timestamp is more than a few minutes old so an old delivery cannot be
 * replayed at you.
 */
export async function verifySignature(
  secret: string,
  timestamp: string,
  body: string,
  signatureHeader: string,
  maxAgeSeconds = 300,
): Promise<boolean> {
  const age = Math.abs(Date.now() / 1000 - Number(timestamp));
  if (!Number.isFinite(age) || age > maxAgeSeconds) return false;

  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const mac = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode(`${timestamp}.${body}`),
  );
  const expected =
    "sha256=" +
    Array.from(new Uint8Array(mac))
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");

  // Constant time, so the comparison itself does not leak the answer.
  if (expected.length !== signatureHeader.length) return false;
  let diff = 0;
  for (let i = 0; i < expected.length; i++) {
    diff |= expected.charCodeAt(i) ^ signatureHeader.charCodeAt(i);
  }
  return diff === 0;
}
