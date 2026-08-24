<script lang="ts">
  import { onMount } from "svelte";
  import { page } from "$app/state";
  import { api, statusLabel, when, ApiError, type Task, type Run } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";

  const id = page.params.id!;
  let task = $state<Task | null>(null);
  let runs = $state<Run[]>([]);
  let plan = $state<any>(null);
  let problem = $state<string | null>(null);
  let busy = $state(false);

  async function load() {
    try {
      [task, runs, plan] = await Promise.all([api.task(id), api.runs(id), api.playbook(id)]);
      problem = null;
    } catch (e) {
      problem = e instanceof ApiError ? e.message : String(e);
    }
  }
  onMount(load);

  async function act(fn: () => Promise<unknown>) {
    busy = true;
    try { await fn(); await load(); }
    catch (e) { problem = e instanceof ApiError ? e.message : String(e); }
    finally { busy = false; }
  }
</script>

{#if problem}
  <div class="err"><h3>Errand could not do that</h3><div>{problem}</div></div>
{/if}

{#if task}
  <div class="row spread">
    <h1>{task.emoji ?? ""} {task.name}</h1>
    <Hint id="task.status"><span class="pill">{statusLabel(task.status)}</span></Hint>
  </div>
  <p class="deck">{task.description}</p>

  <div class="row" style="flex-wrap:wrap; gap:8px">
    {#if !plan?.active}
      <Hint id="task.teach">
        <button class="primary" disabled={busy} onclick={() => act(() => api.teach(id))}>
          Teach it once
        </button>
      </Hint>
    {:else}
      <Hint id="task.run_now">
        <button class="primary" disabled={busy} onclick={() => act(() => api.run(id))}>Run now</button>
      </Hint>
      <Hint id="task.dry_run">
        <button disabled={busy} onclick={() => act(() => api.run(id, true))}>Rehearse</button>
      </Hint>
      {#if task.status === "paused"}
        <Hint id="task.pause">
          <button disabled={busy} onclick={() => act(() => api.resume(id))}>Resume</button>
        </Hint>
      {:else}
        <Hint id="task.pause">
          <button disabled={busy} onclick={() => act(() => api.pause(id))}>Pause</button>
        </Hint>
      {/if}
      <Hint id="task.activate">
        <button disabled={busy} onclick={() => act(() => api.activate(id))}>Put on a schedule</button>
      </Hint>
    {/if}
  </div>

  {#if task.auto_paused}
    <div class="err" style="margin-top:16px">
      <h3>This task needs you</h3>
      <div>Errand paused it: {task.paused_reason}</div>
      {#if task.paused_reason?.includes("verification") || task.paused_reason?.includes("interrupted")}
        <p class="muted" style="margin:8px 0">
          A run began something that cannot be undone and stopped before confirming it. Check the
          site, then tell Errand what you found.
        </p>
        <div class="row">
          <Hint id="hold.resolve">
            <button disabled={busy} onclick={() => act(() => api.resolveHold(id, "did_not_happen"))}>
              It did not happen
            </button>
          </Hint>
          <Hint id="hold.resolve">
            <button disabled={busy} onclick={() => act(() => api.resolveHold(id, "already_happened"))}>
              It already happened
            </button>
          </Hint>
        </div>
      {/if}
    </div>
  {/if}

  <h2>What it is allowed to do</h2>
  <div class="card">
    <div class="row spread">
      <Hint id="task.allowed_sites"><span>Sites it may open</span></Hint>
      <span class="muted">
        {task.allowed_domains?.length ? task.allowed_domains.join(", ") : "none yet, so it cannot browse"}
      </span>
    </div>
    <div class="row spread" style="margin-top:8px">
      <Hint id="task.limits"><span>Limits on a single run</span></Hint>
      <span class="muted">stops if it runs too long or spends too much</span>
    </div>
  </div>

  <h2>How it does this job</h2>
  {#if plan?.active}
    <Hint id="playbook.what">
      <span class="pill ok">Approved, version {plan.active.version}</span>
    </Hint>
    <pre class="card" style="white-space:pre-wrap; font-size:12.5px; margin-top:8px">{plan.active.markdown}</pre>
  {:else if plan?.versions?.length}
    <div class="card">
      <strong>Waiting for you to read it</strong>
      <p class="muted" style="margin:6px 0 10px">{plan.note}</p>
      {#each plan.versions as v}
        <div class="row spread" style="margin-top:8px">
          <span>Version {v.version} <span class="muted">from {v.source}</span></span>
          <Hint id="playbook.approve">
            <button class="primary" disabled={busy}
              onclick={() => act(() => api.approvePlaybook(id, v.version))}>Approve</button>
          </Hint>
        </div>
      {/each}
    </div>
  {:else}
    <div class="card">
      <p class="muted" style="margin:0">
        Nothing yet. Teach it once and it will write down what worked.
      </p>
    </div>
  {/if}

  <h2>Runs</h2>
  {#if runs.length === 0}
    <p class="muted">It has not run yet.</p>
  {:else}
    {#each runs.slice(0, 12) as r}
      <a class="plain" href={`/run/${r.id}`} data-hint-exempt="opens the run shown in this card">
        <div class="card">
          <div class="row spread">
            <div>
              <span class="pill {r.status === 'succeeded' ? 'ok' : r.status === 'failed' ? 'bad' : ''}">
                {statusLabel(r.status)}
              </span>
              {#if r.mode === "dry_run"}<span class="pill warn">rehearsal</span>{/if}
              <div class="muted" style="margin-top:6px">
                {(r.summary ?? r.failure?.plain_reason ?? "").split("\n")[0].slice(0, 110)}
              </div>
            </div>
            <span class="muted">{when(r.created_at)}</span>
          </div>
        </div>
      </a>
    {/each}
  {/if}
{:else if !problem}
  <p class="muted">Loading…</p>
{/if}
