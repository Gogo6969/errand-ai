/**
 * Talking to the daemon.
 *
 * Every call goes through a Tauri command rather than fetch, so the API token
 * stays in Rust and never enters the webview. A token in JavaScript is a token
 * in the page, readable by anything that ends up running there, and this one
 * can start runs and read your task history.
 */
import { Channel, invoke } from "@tauri-apps/api/core";

export interface Task {
  id: string;
  name: string;
  emoji?: string | null;
  description: string;
  status: "draft" | "teaching" | "ready" | "paused" | "archived";
  next_run_at?: string | null;
  paused_reason?: string | null;
  auto_paused: boolean;
  playbook_version?: number | null;
  allowed_domains: string[];
  schedule?: unknown;
  notify?: Record<string, boolean>;
  limits?: Record<string, number>;
  /** The engine's own words for the schedule. Never interpreted in the browser. */
  schedule_describes?: string;
  schedule_preview?: string[];
}

export interface Failure {
  code: string;
  plain_reason: string;
  technical?: string;
}

export interface Run {
  id: string;
  task_id: string;
  status: string;
  mode: string;
  trigger: string;
  summary?: string | null;
  failure?: Failure | null;
  cost_usd: number;
  started_at?: string | null;
  finished_at?: string | null;
  created_at: string;
}

export interface Step {
  seq: number;
  ts: string;
  kind: string;
  title: string;
  ok: boolean;
}

export interface Health {
  status: string;
  version: string;
  busy_runs: number;
  keychain: string;
  db: string;
}

export interface ChannelHealth {
  channel: string;
  status: string;
  detail: string;
  fix?: string;
}

/** An error the daemon reported, with its plain-language detail. */
export class ApiError extends Error {
  constructor(readonly code: string, message: string) {
    super(message);
  }
}

/**
 * Turn whatever came back from the daemon into an error a person can read.
 *
 * Kept separate from the call itself for a reason that cost an afternoon: when
 * the throw lived inside the same try block as the JSON.parse, its own catch
 * caught it, and every failure in the whole app reached the screen as raw JSON
 * instead of the sentence the daemon had carefully written.
 */
function asApiError(e: unknown): ApiError {
  const text = String(e);
  let parsed: { code?: string; detail?: string; title?: string } | null = null;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = null;
  }
  if (parsed && typeof parsed === "object") {
    return new ApiError(parsed.code ?? "unknown", parsed.detail ?? parsed.title ?? text);
  }
  // Not JSON at all, so it did not come from the daemon: the window could not
  // reach it, or something threw before the request was made.
  return new ApiError("unreachable", text.replace(/^Error:\s*/, ""));
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  let raw: string;
  try {
    raw = await invoke<string>("api", {
      method,
      path,
      body: body ? JSON.stringify(body) : null,
    });
  } catch (e) {
    throw asApiError(e);
  }
  try {
    return JSON.parse(raw) as T;
  } catch {
    // The request succeeded and the reply is not JSON. Nothing sensible to do
    // with it, but say so rather than letting a parser error surface.
    throw new ApiError(
      "unreadable",
      "Errand's background service answered with something this window could not read.",
    );
  }
}

/**
 * Follow a run as it happens.
 *
 * The page cannot open the daemon's event stream itself — that would mean
 * holding the API token in JavaScript — so Rust holds it and pushes each event
 * down a channel. Returns a function that stops listening.
 */
export function followRun(runId: string, onEvent: (e: { event: string; data: unknown }) => void) {
  const channel = new Channel<string>();
  let stopped = false;
  channel.onmessage = (raw) => {
    if (stopped) return;
    try { onEvent(JSON.parse(raw)); }
    catch { /* a frame we do not understand is not worth breaking the page over */ }
  };
  invoke("follow_run", { runId, onEvent: channel }).catch(() => {
    // The stream ending is normal: the run finished, or the window moved on.
  });
  return () => { stopped = true; };
}

export const api = {
  health: () => call<Health>("GET", "/v1/health/detail"),
  tasks: () => call<{ items: Task[] }>("GET", "/v1/tasks").then((r) => r.items),
  task: (id: string) => call<Task>("GET", `/v1/tasks/${id}`),
  createTask: (name: string, description: string, emoji?: string, allowed_domains?: string[]) =>
    call<Task>("POST", "/v1/tasks", { name, description, emoji, allowed_domains }),
  teach: (id: string) => call<Run>("POST", `/v1/tasks/${id}/teach`),
  run: (id: string, dryRun = false) => call<Run>("POST", `/v1/tasks/${id}/run`, { dry_run: dryRun }),
  pause: (id: string, reason?: string) => call("POST", `/v1/tasks/${id}/pause`, { reason }),
  resume: (id: string) => call("POST", `/v1/tasks/${id}/resume`),
  activate: (id: string) => call("POST", `/v1/tasks/${id}/activate`),
  resolveHold: (id: string, outcome: "already_happened" | "did_not_happen") =>
    call("POST", `/v1/tasks/${id}/holds`, { outcome }),

  runs: (taskId?: string) =>
    call<{ items: Run[] }>("GET", taskId ? `/v1/runs?task_id=${taskId}` : "/v1/runs").then((r) => r.items),
  runDetail: (id: string) => call<Run & { steps: Step[] }>("GET", `/v1/runs/${id}`),

  playbook: (id: string) =>
    call<{ active: { version: number; goal: string; markdown: string } | null; versions: any[]; note: string }>(
      "GET",
      `/v1/tasks/${id}/playbook`,
    ),
  approvePlaybook: (id: string, version: number) =>
    call("POST", `/v1/tasks/${id}/playbook/${version}/approve`),

  credentials: () => call<{ items: any[] }>("GET", "/v1/credentials").then((r) => r.items),
  addCredential: (label: string, domain: string, username: string, secret: string) =>
    call("POST", "/v1/credentials", { label, domain, username, secret }),
  deleteCredential: (id: string) => call("DELETE", `/v1/credentials/${id}`),

  channels: () => call<{ channels: ChannelHealth[]; notes: Record<string, string> }>("GET", "/v1/channels"),
  testChannel: (c: string) => call("POST", `/v1/channels/${c}/test`),
  enableChannel: (c: string) => call<ChannelHealth>("POST", `/v1/channels/${c}/enable`),
  configureChannel: (
    c: string,
    secrets: Record<string, string>,
    settings: Record<string, unknown> = {},
  ) => call("POST", `/v1/channels/${c}/config`, { secrets, settings }),
  settings: () => call<Record<string, unknown>>("GET", "/v1/settings"),

  patchTask: (id: string, patch: TaskPatch) => call<PatchResult>("PATCH", `/v1/tasks/${id}`, patch),
  previewSchedule: (schedule: unknown) =>
    call<SchedulePreview>("POST", "/v1/schedule/preview", schedule),

  recipients: () => call<{ items: Recipient[] }>("GET", "/v1/recipients").then((r) => r.items),
  addRecipient: (label: string, channel: string, address: string) =>
    call<Recipient>("POST", "/v1/recipients", { label, channel, address }),
  deleteRecipient: (id: string) => call("DELETE", `/v1/recipients/${id}`),
  taskRecipients: (taskId: string) =>
    call<{ items: TaskRecipient[] }>("GET", `/v1/tasks/${taskId}/recipients`).then((r) => r.items),
  linkRecipient: (taskId: string, recipientId: string, onSuccess: boolean, onFailure: boolean) =>
    call("POST", `/v1/tasks/${taskId}/recipients`, {
      recipient_id: recipientId, on_success: onSuccess, on_failure: onFailure,
    }),
  unlinkRecipient: (taskId: string, recipientId: string) =>
    call("DELETE", `/v1/tasks/${taskId}/recipients/${recipientId}`),

  ai: () => call<AiSetup>("GET", "/v1/ai"),
  aiCatalogue: () => call<{ services: KnownService[] }>("GET", "/v1/ai/catalogue").then((r) => r.services),
  saveProvider: (p: SaveProvider) => call<{ id: string; health: string; health_detail: string }>("POST", "/v1/ai/providers", p),
  removeProvider: (id: string) => call("DELETE", `/v1/ai/providers/${id}`),
  testProvider: (id: string) => call<{ health: string; health_detail: string }>("POST", `/v1/ai/providers/${id}/test`),
  discoverProviders: (scanNetwork = false) =>
    call<{ found: Provider[]; looked_at: string; note: string }>(
      "POST",
      `/v1/ai/discover?scan_network=${scanNetwork}`,
    ),
  bindRole: (role: string, providerId: string | null) =>
    call("POST", `/v1/ai/roles/${role}`, { provider_id: providerId }),
  setLocalOnly: (enabled: boolean) => call("POST", "/v1/ai/local-only", { enabled }),
  saveAnthropicKey: (key: string) => call("POST", "/v1/ai/anthropic-key", { key }),
};

/** Every field optional: absent means leave it as it was. */
export interface TaskPatch {
  name?: string;
  emoji?: string;
  description?: string;
  schedule?: unknown;
  notify?: unknown;
  limits?: unknown;
  allowed_domains?: string[];
  /** Set after a schedule_change_may_repeat refusal, once the person has read it. */
  acknowledge_repeat?: boolean;
}

export interface PatchResult {
  task: Task;
  /** Things worth saying but not worth refusing over, e.g. www without the apex. */
  warnings?: string[];
}

/** What the engine says a schedule really means. Never computed in the browser. */
export interface SchedulePreview {
  valid: boolean;
  describes: string;
  preview: string[];
  problem?: string;
}

export interface Recipient {
  id: string;
  label: string;
  channel: string;
  address: string;
}

export interface TaskRecipient extends Recipient {
  on_success: boolean;
  on_failure: boolean;
}

export interface Provider {
  id: string;
  kind: "claude_cli" | "anthropic_api" | "openai_compat";
  label: string;
  base_url: string | null;
  model: string | null;
  enabled: boolean;
  discovered: boolean;
  health: string | null;
  health_detail: string | null;
}

export interface SaveProvider {
  id?: string;
  /** One of the names Errand knows, e.g. "openai". Fills in the rest. */
  known?: string;
  kind?: string;
  label?: string;
  base_url?: string;
  model?: string;
  /** Write-only. Goes to the keychain and never comes back. */
  key?: string;
  enabled?: boolean;
}

/** A service Errand can talk to without being told its address. */
export interface KnownService {
  id: string;
  name: string;
  base_url: string;
  key_prefix: string;
  keys_url: string;
  example_model: string;
  needs_key: boolean;
}

/** What each of Errand's four jobs would use if it were asked right now. */
export interface RoleSetup {
  role: string;
  explains: string;
  needs_agentic: boolean;
  /** False when Errand does not consult this role yet. */
  in_use: boolean;
  not_used_because: string | null;
  chosen: string | null;
  using: { id: string; label: string; model: string; local: boolean } | null;
  fallbacks: string[];
  problem: string | null;
}

export interface AiSetup {
  providers: Provider[];
  roles: RoleSetup[];
  local_only: boolean;
}

/** Human wording for a status, so no screen has to invent its own. */
export function statusLabel(s: string): string {
  return (
    {
      draft: "Not taught yet",
      teaching: "Learning",
      ready: "Armed",
      paused: "Paused",
      archived: "Archived",
      succeeded: "Done",
      failed: "Could not finish",
      running: "Running",
      queued: "Waiting to start",
      healing: "Trying a different way",
      skipped: "Skipped",
      cancelled: "Cancelled",
    }[s] ?? s
  );
}

/** "in 2 hours", "just now". Absolute time belongs in the tooltip. */
export function when(iso?: string | null): string {
  if (!iso) return "";
  const d = new Date(iso).getTime();
  if (Number.isNaN(d)) return "";
  const diff = d - Date.now();
  const abs = Math.abs(diff);
  const mins = Math.round(abs / 60000);
  const unit =
    mins < 1 ? "less than a minute" :
    mins < 60 ? `${mins} minute${mins === 1 ? "" : "s"}` :
    abs < 86400000 ? `${Math.round(mins / 60)} hour${Math.round(mins / 60) === 1 ? "" : "s"}` :
    `${Math.round(mins / 1440)} day${Math.round(mins / 1440) === 1 ? "" : "s"}`;
  return diff > 0 ? `in ${unit}` : `${unit} ago`;
}
