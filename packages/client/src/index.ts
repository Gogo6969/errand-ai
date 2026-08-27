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
  /**
   * Armed irreversible actions waiting on a person to say what happened. When
   * this is above zero the task is blocked until someone resolves the hold.
   */
  open_holds?: number;
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
  /**
   * Was everything irreversible recorded rather than done? Read this rather
   * than the mode: a teach run can be a rehearsal too, and its mode still says
   * "teach" because learning is what it is for.
   */
  rehearsal: boolean;
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
  /** Set when the step left a file behind; fetch it with getArtifact. */
  artifact_id?: string | null;
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

  /**
   * One file a run left behind, such as a screenshot, fetched by id.
   *
   * The id comes from a step's `artifact_id`. There is no list endpoint:
   * artifacts are addressed by id and never by path, so a file you were not
   * given the id of does not exist.
   */
  async getArtifact(id: string): Promise<{ contentType: string; bytes: ArrayBuffer }> {
    const res = await fetch(`${this.baseUrl}/v1/artifacts/${id}`, {
      headers: { Authorization: `Bearer ${this.token}` },
    });
    if (!res.ok) {
      const text = await res.text();
      let code = "unknown";
      let detail = `Errand returned ${res.status}`;
      try {
        const json = JSON.parse(text);
        code = json.code ?? code;
        detail = json.detail ?? json.title ?? detail;
      } catch {
        // Not JSON; the status code is the whole story.
      }
      throw new ErrandError(res.status, code, detail);
    }
    return {
      contentType: res.headers.get("content-type") ?? "application/octet-stream",
      bytes: await res.arrayBuffer(),
    };
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

  // ------------------------------------------------------ configuring a task --

  /**
   * Change a task. Every field is optional; anything you leave out is left
   * alone, so you never have to read a task in order to edit one field of it.
   *
   * Two refusals are worth handling rather than surfacing raw:
   * `task_not_taught` means the task has no approved plan, so it cannot be put
   * on a schedule; `schedule_change_may_repeat` means the new schedule's first
   * run comes sooner than the old one's would have, and something irreversible
   * has already been done for this slot. Pass `acknowledge_repeat: true` only
   * once a person has actually read that.
   */
  async updateTask(id: string, patch: TaskPatch): Promise<{ task: Task; warnings?: string[] }> {
    return this.request("PATCH", `/v1/tasks/${id}`, patch);
  }

  /**
   * What a schedule really means, and when it would next run.
   *
   * Worth calling before saving one. A schedule expression that parses can
   * still say something other than what was meant, and this is the engine's own
   * answer rather than your interpretation of it.
   */
  async previewSchedule(schedule: unknown): Promise<SchedulePreview> {
    return this.request("POST", "/v1/schedule/preview", schedule);
  }

  // ---------------------------------------------------------------- people --

  async listRecipients(): Promise<Recipient[]> {
    const r = await this.request<{ items: Recipient[] }>("GET", "/v1/recipients");
    return r.items;
  }

  async addRecipient(label: string, channel: Channel, address: string): Promise<Recipient> {
    return this.request("POST", "/v1/recipients", { label, channel, address });
  }

  async removeRecipient(id: string): Promise<void> {
    await this.request("DELETE", `/v1/recipients/${id}`);
  }

  async taskRecipients(taskId: string): Promise<TaskRecipient[]> {
    const r = await this.request<{ items: TaskRecipient[] }>(
      "GET",
      `/v1/tasks/${taskId}/recipients`,
    );
    return r.items;
  }

  /**
   * Let one task message one person. This is the grant that makes messaging
   * possible at all, and it needs the `approve` scope rather than `manage`,
   * because it decides whether a real message reaches a real third party.
   */
  async allowTaskToMessage(
    taskId: string,
    recipientId: string,
    when: { onSuccess?: boolean; onFailure?: boolean } = {},
  ): Promise<void> {
    await this.request("POST", `/v1/tasks/${taskId}/recipients`, {
      recipient_id: recipientId,
      on_success: when.onSuccess ?? true,
      on_failure: when.onFailure ?? true,
    });
  }

  async stopTaskMessaging(taskId: string, recipientId: string): Promise<void> {
    await this.request("DELETE", `/v1/tasks/${taskId}/recipients/${recipientId}`);
  }

  // ------------------------------------------------------------- the machine --

  /** Which model would do each job, and whether anything leaves the machine. */
  async ai(): Promise<AiSetup> {
    return this.request("GET", "/v1/ai");
  }

  /**
   * Logins Errand may use. Secrets are write-only: this returns what each one
   * is for and which site it is bound to, never the value.
   */
  async listCredentials(): Promise<Credential[]> {
    const r = await this.request<{ items: Credential[] }>("GET", "/v1/credentials");
    return r.items;
  }

  async addCredential(c: {
    label: string;
    domain: string;
    username?: string;
    secret: string;
  }): Promise<{ id: string }> {
    return this.request("POST", "/v1/credentials", c);
  }

  async removeCredential(id: string): Promise<void> {
    await this.request("DELETE", `/v1/credentials/${id}`);
  }

  /** How each way of reaching you is doing, and what to do about any that is not. */
  async channels(): Promise<{ channels: ChannelHealth[]; notes: Record<string, string> }> {
    return this.request("GET", "/v1/channels");
  }

  async settings(): Promise<Record<string, unknown>> {
    return this.request("GET", "/v1/settings");
  }

  /**
   * Mint a key for another program. Give each one only the scopes it needs.
   * The key is returned once and stored only as a hash, so it cannot be shown
   * again. If you lose it, mint another and revoke this one.
   */
  async createToken(name: string, scopes: Scope[]): Promise<{ id: string; token: string }> {
    return this.request("POST", "/v1/tokens", { name, scopes: scopes.join(",") });
  }

  async revokeToken(id: string): Promise<void> {
    await this.request("DELETE", `/v1/tokens/${id}`);
  }
}

export type Channel = "telegram" | "whatsapp" | "apple_mail" | "imessage";

/** Every field optional: absent means leave it as it is. */
export interface TaskPatch {
  name?: string;
  emoji?: string;
  description?: string;
  /** A ScheduleSpec. Check it with previewSchedule first. */
  schedule?: unknown;
  notify?: Record<string, boolean>;
  limits?: Record<string, number>;
  /** Bare hosts. A full URL is accepted and tidied; a wildcard is refused. */
  allowed_domains?: string[];
  acknowledge_repeat?: boolean;
}

export interface SchedulePreview {
  valid: boolean;
  /** The schedule in plain English, from the engine that will run it. */
  describes: string;
  /** The next few moments it would fire, as ISO timestamps. */
  preview: string[];
  problem?: string | null;
}

export interface Recipient {
  id: string;
  label: string;
  channel: Channel;
  address: string;
  address_masked?: string;
}

export interface TaskRecipient extends Recipient {
  on_success: boolean;
  on_failure: boolean;
}

export interface Credential {
  id: string;
  label: string;
  domain: string;
  username?: string | null;
  use_count: number;
}

export interface ChannelHealth {
  channel: string;
  status: string;
  detail: string;
  fix?: string | null;
}

export interface AiSetup {
  providers: {
    id: string;
    kind: string;
    label: string;
    base_url?: string | null;
    model?: string | null;
    enabled: boolean;
    health?: string | null;
    health_detail?: string | null;
  }[];
  roles: {
    role: string;
    explains: string;
    in_use: boolean;
    not_used_because?: string | null;
    using?: { id: string; label: string; model: string; local: boolean } | null;
    fallbacks: string[];
    problem?: string | null;
  }[];
  local_only: boolean;
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
