<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import { api, artifactUrl, followRun, statusLabel, ApiError } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";

  const id = page.params.id!;
  let run = $state<any>(null);
  let problem = $state<string | null>(null);
  let retrying = $state(false);
  // Screenshots load when their step is opened, not before: a run can take
  // dozens, and most of them nobody looks at.
  let shots = $state<Record<string, string | null>>({});
  let stop: (() => void) | undefined;
  let timer: any;

  const FINISHED = ["succeeded", "failed", "cancelled", "skipped"];

  async function load() {
    try { run = await api.runDetail(id); problem = null; }
    catch (e) { problem = e instanceof ApiError ? e.message : String(e); }
  }

  async function retry() {
    retrying = true;
    try {
      const fresh = await api.run(run.task_id);
      // The run that just started is the one worth watching, not the failure
      // that provoked it.
      goto(`/run/${fresh.id}`);
    } catch (e) {
      problem = e instanceof ApiError ? e.message : String(e);
      retrying = false;
    }
  }

  async function showShot(artifactId: string) {
    if (artifactId in shots) return;
    try { shots[artifactId] = await artifactUrl(artifactId); }
    catch { shots[artifactId] = null; }
  }

  onMount(() => {
    load();
    // Each step arrives as it happens rather than up to three seconds later,
    // which matters most while you are watching a task being taught.
    stop = followRun(id, () => load());

    // A slow safety net, not the mechanism. If the stream drops, the daemon
    // restarts mid-run, say, a stalled page would otherwise look like a
    // stalled run, which is the more alarming of the two.
    timer = setInterval(() => {
      if (!run || FINISHED.includes(run.status)) return;
      load();
    }, 20000);
  });

  onDestroy(() => {
    stop?.();
    clearInterval(timer);
  });
</script>

{#if problem}<div class="err"><h3>Could not load that run</h3><div>{problem}</div></div>{/if}

{#if run}
  <div class="row spread">
    <h1>Run</h1>
    <Hint id="run.status">
      <span class="pill {run.status === 'succeeded' ? 'ok' : run.status === 'failed' ? 'bad' : ''}">
        {statusLabel(run.status)}
      </span>
    </Hint>
  </div>
  <p class="deck">
    <a class="plain" href={`/task/${run.task_id}`} data-hint-exempt="link back to the task, labelled by its text">
      Back to the task
    </a>
    ·
    <Hint id="run.cost"><span class="muted">${run.cost_usd.toFixed(2)}</span></Hint>
  </p>

  {#if run.failure}
    <div class="err">
      <h3>It could not finish</h3>
      <Hint id="run.failure">
        <div style="white-space:pre-wrap">{run.failure.plain_reason}</div>
      </Hint>
      {#if run.failure.technical}
        <details style="margin-top:10px">
          <summary class="muted" data-hint-exempt="discloses technical detail, labelled by its text">
            Technical detail
          </summary>
          <pre class="muted" style="white-space:pre-wrap; font-size:12px">{run.failure.technical}</pre>
        </details>
      {/if}
      <div class="row" style="margin-top:10px">
        <Hint id="run.retry">
          <button disabled={retrying} onclick={retry}>Try again</button>
        </Hint>
      </div>
    </div>
  {:else if run.summary}
    <div class="card"><strong>What it did</strong><div style="margin-top:4px">{run.summary}</div></div>
  {/if}

  <h2>Step by step</h2>
  <Hint id="run.timeline"><span class="muted">{run.steps.length} steps</span></Hint>
  <div style="margin-top:10px">
    {#each run.steps as s}
      <div class="card" style="padding:9px 14px; margin-bottom:6px; border-left:3px solid {s.ok ? 'var(--rule)' : 'var(--bad)'}">
        <div class="row spread">
          <span>{s.title}</span>
          <span class="muted">{s.kind}</span>
        </div>
        {#if s.artifact_id}
          {@const aid = s.artifact_id}
          <details
            style="margin-top:6px"
            ontoggle={(e) => {
              if ((e.currentTarget as HTMLDetailsElement).open) showShot(aid);
            }}
          >
            <summary class="muted" data-hint-exempt="shows the screenshot this step took, labelled by its text">
              See what it saw
            </summary>
            {#if shots[aid]}
              <img class="shot" src={shots[aid]} alt="The page as the agent saw it at this step" />
            {:else if shots[aid] === null}
              <p class="muted">That screenshot is no longer there.</p>
            {:else}
              <p class="muted">Loading…</p>
            {/if}
          </details>
        {/if}
      </div>
    {/each}
  </div>
{/if}

<style>
  .shot {
    display: block; max-width: 100%; margin-top: 8px;
    border: 1px solid var(--rule); border-radius: 6px;
  }
</style>
