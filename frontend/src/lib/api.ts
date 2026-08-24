/**
 * Talking to the daemon.
 *
 * Every call goes through a Tauri command rather than fetch, so the API token
 * stays in Rust and never enters the webview. A token in JavaScript is a token
 * in the page, readable by anything that ends up running there, and this one
 * can start runs and read your task history.
 */
import { invoke } from "@tauri-apps/api/core";

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

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  try {
    const raw = await invoke<string>("api", { method, path, body: body ? JSON.stringify(body) : null });
    return JSON.parse(raw) as T;
  } catch (e) {
    const text = String(e);
    try {
      const j = JSON.parse(text);
      throw new ApiError(j.code ?? "unknown", j.detail ?? j.title ?? text);
    } catch {
      throw new ApiError("unreachable", text);
    }
  }
}

export const api = {
  health: () => call<Health>("GET", "/v1/health/detail"),
  tasks: () => call<{ items: Task[] }>("GET", "/v1/tasks").then((r) => r.items),
  task: (id: string) => call<Task>("GET", `/v1/tasks/${id}`),
  createTask: (name: string, description: string, emoji?: string) =>
    call<Task>("POST", "/v1/tasks", { name, description, emoji }),
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
  configureChannel: (c: string, secrets: Record<string, string>) =>
    call("POST", `/v1/channels/${c}/config`, { secrets }),
};

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
