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
  /**
   * Which model carries this task out, by its id in the AI screen's list.
   *
   * Null means the task has not said, so it follows the default. Worth setting
   * where it matters: whichever model does the work is the one that reads
   * whatever the task reads.
   */
  model_id?: string | null;
  schedule?: unknown;
  notify?: Record<string, boolean>;
  limits?: Record<string, number>;
  /** Armed irreversible actions waiting on a person to say what happened. */
  open_holds?: number;
  /** The engine's own words for the schedule. Never interpreted in the browser. */
  schedule_describes?: string;
  schedule_preview?: string[];
  /** The newest run, so a screen can say what really happened. */
  last_run?: Run | null;
}

export interface Failure {
  code: string;
  plain_reason: string;
  technical?: string;
}

/** Somewhere a run also put its answer, because the task asked for a copy. */
export interface AnswerCopy {
  id: string;
  /** "note" | "file" | "message" */
  kind: string;
  /** What to call it on screen: a note title, a file name, a person. */
  label: string;
}

export interface Run {
  id: string;
  task_id: string;
  status: string;
  mode: string;
  /**
   * Was everything irreversible recorded rather than done? Separate from the
   * mode, because a teach run can be a rehearsal too: read this, never the mode.
   */
  rehearsal?: boolean;
  trigger: string;
  /** One line about the work: where it went, what it had to do. */
  summary?: string | null;
  /**
   * What the run produced: the thing the task was set up to get.
   *
   * Separate from the summary because they are separate things, and for a long
   * while only the summary existed, so a task that was asked to find something
   * out reported the filing and not the finding.
   */
  answer?: string | null;
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
  /** Set on steps that left a file behind, such as a screenshot. */
  artifact_id?: string | null;
}

export interface Health {
  status: string;
  version: string;
  busy_runs: number;
  keychain: string;
  db: string;
}

export interface ChannelHealth {
  /** The internal id, e.g. "imessage". For URLs and settings keys, never for reading. */
  channel: string;
  /** What a person calls it: "Apple Messages". This is what goes on screen. */
  display_name: string;
  status: string;
  detail: string;
  fix?: string;
  /** Where Errand reaches you on this channel, or null while nobody has said. */
  self_address?: string | null;
}

/**
 * One app on this Mac Errand may drive, and whether macOS will let it.
 *
 * Not a channel: nothing here messages anybody. The fields match ChannelHealth
 * where they mean the same thing, so a card can be drawn the same way, and
 * there is no self_address because this is not a way of reaching anyone.
 */
export interface AutomationApp {
  /** The internal id, e.g. "mail_reading". For URLs, never for reading. */
  app: string;
  /** What a person calls it: "Apple Notes". This is what goes on screen. */
  display_name: string;
  status: string;
  detail: string;
  fix?: string;
}

/**
 * The name to put on screen for a channel.
 *
 * The daemon sends display_name, but the window and the daemon are updated
 * separately, and an older daemon sending nothing would put the word "undefined"
 * where the channel's name belongs. Falling back to the id at least says
 * something true.
 */
export function channelName(c: { channel: string; display_name?: string }): string {
  return c.display_name || c.channel.replace(/_/g, " ");
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
 * The page cannot open the daemon's event stream itself, since that would mean
 * holding the API token in JavaScript, so Rust holds it and pushes each event
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

/**
 * One screenshot a run took, as a data URL.
 *
 * Images are bytes, so they cannot ride the ordinary call path; Rust fetches
 * them and hands back a data URL, and the token never leaves Rust to do it.
 */
export async function artifactUrl(id: string): Promise<string> {
  return invoke<string>("artifact", { id });
}

export const api = {
  health: () => call<Health>("GET", "/v1/health/detail"),
  tasks: () => call<{ items: Task[] }>("GET", "/v1/tasks").then((r) => r.items),
  task: (id: string) => call<Task>("GET", `/v1/tasks/${id}`),
  /**
   * Create a task. The name is optional: leave it empty and Errand works one
   * out from the description, the same way it works out which sites the job
   * needs. The reply carries `set_up`, which is what it decided and why.
   */
  createTask: (name: string, description: string, emoji?: string, allowed_domains?: string[]) =>
    call<Task & { set_up?: { what: string; because: string }[] }>("POST", "/v1/tasks", {
      name: name.trim() || undefined,
      description,
      emoji,
      // Sent only when the person named some, so an empty list is not mistaken
      // for "this task may open nothing".
      allowed_domains: allowed_domains?.length ? allowed_domains : undefined,
    }),
  teach: (id: string, rehearse = false) =>
    call<Run>("POST", `/v1/tasks/${id}/teach`, { dry_run: rehearse }),
  run: (id: string, dryRun = false) => call<Run>("POST", `/v1/tasks/${id}/run`, { dry_run: dryRun }),
  pause: (id: string, reason?: string) => call("POST", `/v1/tasks/${id}/pause`, { reason }),
  resume: (id: string) => call("POST", `/v1/tasks/${id}/resume`),
  activate: (id: string) => call("POST", `/v1/tasks/${id}/activate`),
  resolveHold: (id: string, outcome: "already_happened" | "did_not_happen") =>
    call("POST", `/v1/tasks/${id}/holds`, { outcome }),

  runs: (taskId?: string) =>
    call<{ items: Run[] }>("GET", taskId ? `/v1/runs?task_id=${taskId}` : "/v1/runs").then((r) => r.items),
  runDetail: (id: string) =>
    call<Run & { steps: Step[]; answer_copies: AnswerCopy[] }>("GET", `/v1/runs/${id}`),
  /** Show the person a copy of an answer where the run also put it. */
  openAnswerCopy: (id: string) => call("POST", `/v1/answer-copies/${id}/open`),

  playbook: (id: string) =>
    call<{
      active: { version: number; goal: string; markdown: string } | null;
      versions: {
        version: number;
        source: string;
        approved: boolean;
        changelog: string | null;
        created_at: string;
        /** The plan itself. Present whether or not anybody has approved it. */
        markdown: string | null;
      }[];
      note: string;
    }>(
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
  testChannel: (c: string) =>
    call<{ queued: string; sent_to: string; note: string }>("POST", `/v1/channels/${c}/test`),
  enableChannel: (c: string) => call<ChannelHealth>("POST", `/v1/channels/${c}/enable`),

  // Asking macOS is the same act as checking, so loading this is what puts the
  // prompt on the screen. That is the point: somebody is looking at it now.
  automation: () =>
    call<{ apps: AutomationApp[]; notes: Record<string, string> }>("GET", "/v1/automation"),
  enableAutomation: (app: string) =>
    call<AutomationApp>("POST", `/v1/automation/${app}/enable`),
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

  mailGrant: (taskId: string) => call<MailGrant>("GET", `/v1/tasks/${taskId}/mail`),
  grantMail: (taskId: string, mayFile: boolean) =>
    call<MailGrant>("POST", `/v1/tasks/${taskId}/mail`, { may_file: mayFile }),
  revokeMail: (taskId: string) => call("DELETE", `/v1/tasks/${taskId}/mail`),

  ai: () => call<AiSetup>("GET", "/v1/ai"),
  aiCatalogue: () => call<{ services: KnownService[] }>("GET", "/v1/ai/catalogue").then((r) => r.services),
  saveProvider: (p: SaveProvider) => call<{ id: string; health: string; health_detail: string }>("POST", "/v1/ai/providers", p),
  removeProvider: (id: string) => call("DELETE", `/v1/ai/providers/${id}`),
  testProvider: (id: string) =>
    call<{ health: string; health_detail: string; tools: string; tools_says: string }>(
      "POST",
      `/v1/ai/providers/${id}/test`,
    ),
  discoverProviders: (scanNetwork = false) =>
    call<ScanResult>("POST", `/v1/ai/discover?scan_network=${scanNetwork}`),
  bindRole: (role: string, providerId: string | null) =>
    call("POST", `/v1/ai/roles/${role}`, { provider_id: providerId }),
  /** Which Claude does this job. Null puts it back on whichever Errand picks. */
  setRoleModel: (role: string, model: string | null) =>
    call<{ role: string; model: string }>("POST", `/v1/ai/roles/${role}/model`, { model }),
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
  /**
   * Which model carries the task out. Null puts it back on the default.
   *
   * Leaving it out is not the same as sending null, and that difference is
   * load-bearing: every other save on the task page leaves this out, and a task
   * must keep the model it was given when somebody edits its sites.
   */
  model_id?: string | null;
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

/**
 * What one task may do with the person's mail, and where that mail then goes.
 *
 * `where_it_goes` is written by the daemon and shown as it arrives. It is the
 * one sentence in the app that must not drift: it says whether reading somebody's
 * post means sending it off this Mac, and that depends on how the models are set
 * up, which the daemon knows and this screen does not.
 */
export interface MailGrant {
  granted: boolean;
  may_file: boolean;
  granted_at?: string | null;
  local_only: boolean;
  where_it_goes: string;
}

/** What a scan turned up, including what it could not use. */
export interface ScanResult {
  found: Provider[];
  looked_at: string;
  addresses: number;
  ports: number;
  /** Answered, but not usable as it stands, with the reason. */
  also_seen: { url: string; why: string }[];
  /** Set when macOS refused the local network, so an empty result means nothing. */
  blocked?: string | null;
  note: string;
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

/**
 * A model in Errand's own list, with what it has found out about it.
 *
 * Separate from Provider because a model a scan has only just noticed has been
 * asked nothing at all: what follows is knowledge, and knowledge Errand does
 * not have yet must not be able to arrive here looking like an answer.
 */
export interface ListedProvider extends Provider {
  /** Whether it will call a tool, which is all that carrying out a task needs. */
  tools: "yes" | "no" | "unknown";
  /** That, as a standing label in plain words. */
  tools_says: string;
  /** Set only where Errand has actually found it wanting. Never for "not checked". */
  cannot_carry_out_because: string | null;
  /**
   * Which models this endpoint is using, where that is not one answer.
   *
   * Only the Claude command line tool: it answers to three, one per job, so its
   * `model` is empty and this is what says which one is doing what.
   */
  models_in_use: string | null;
}

/** One of the Claude models the command line tool answers to. */
export interface ClaudeModel {
  /** What Errand asks for. An alias, so it survives a new version landing. */
  alias: string;
  name: string;
  what_it_is_for: string;
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
  /** Why the model picked for this job cannot do it, when that turns out to be so. */
  chosen_problem: string | null;
  using: {
    id: string;
    label: string;
    /** Exactly what Errand asks that endpoint for. */
    model: string;
    /** The same model as a person would name it. Null where there is no choice. */
    model_name: string | null;
    /** What using this one means. Null where there is no choice. */
    model_says: string | null;
    /** True only for the Claude command line tool, the one endpoint with a choice. */
    can_choose_model: boolean;
    local: boolean;
  } | null;
  fallbacks: string[];
  problem: string | null;
}

export interface AiSetup {
  providers: ListedProvider[];
  roles: RoleSetup[];
  /** The three the Claude command line tool accepts, so no screen invents its own. */
  claude_models: ClaudeModel[];
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
      // Run states the engine can reach but rarely does. Unmapped they would
      // reach the screen as their own internal names, which is how somebody
      // ends up reading the word "preflight" on a task page.
      armed: "Ready to start",
      preflight: "Getting ready",
      holding: "Waiting on you",
      waiting_input: "Waiting on you",
      takeover: "Needs you at the keyboard",
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
