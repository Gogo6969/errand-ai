<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import Trouble from "$lib/components/Trouble.svelte";
  import { reconnecting } from "$lib/reconnect.svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import { api, artifactUrl, followRun, statusLabel, ApiError } from "$lib/api";
  import Answered from "$lib/components/Answered.svelte";
  import Hint from "$lib/components/Hint.svelte";

  const id = page.params.id!;
  let run = $state<any>(null);
  let problem = $state<string | null>(null);
  let retrying = $state(false);
  let opening = $state<string | null>(null);
  // Screenshots load when their step is opened, not before: a run can take
  // dozens, and most of them nobody looks at.
  let shots = $state<Record<string, string | null>>({});
  let stop: (() => void) | undefined;
  let timer: any;

  const FINISHED = ["succeeded", "failed", "cancelled", "skipped"];

  const conn = reconnecting(
    async () => {
      run = await api.runDetail(id);
    },
    (p) => (problem = p),
  );
  const load = conn.run;

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

  /** Bring up the note or file where this run also left its answer. */
  async function openCopy(c: { id: string; label: string }) {
    opening = c.id;
    try { await api.openAnswerCopy(c.id); problem = null; }
    catch (e) { problem = e instanceof ApiError ? e.message : String(e); }
    finally { opening = null; }
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

{#if problem}
  <Trouble {problem} retrying={conn.retrying} onRetry={() => conn.run(true)} />
{/if}

{#if run}
  <div class="row spread">
    <h1>Run</h1>
    <div class="row" style="gap:6px">
      <!-- Said next to the status, because somebody reading "it booked the
           court" needs to know whether it really did. -->
      {#if run.rehearsal}
        <Hint id="run.rehearsal"><span class="pill warn">rehearsal</span></Hint>
      {/if}
      <Hint id="run.status">
        <span class="pill {run.status === 'succeeded' ? 'ok' : run.status === 'failed' ? 'bad' : ''}">
          {statusLabel(run.status)}
        </span>
      </Hint>
    </div>
  </div>
  <p class="deck">
    <a class="plain" href={`/task/${run.task_id}`} data-hint-exempt="link back to the task, labelled by its text">
      Back to the task
    </a>
    ·
    <Hint id="run.cost"><span class="muted">${run.cost_usd.toFixed(2)}</span></Hint>
  </p>

  <!-- The answer, before anything about the run.
       Outside the failure/summary chain on purpose: a run that read everything,
       worked out the answer and only then found the Mac would not let it write
       the note it was asked for is a failure that still holds the answer, and
       that is the commonest failure there is. -->
  {#if run.answer}
    <div class="card">
      <Hint id="run.answer"><strong>The answer</strong></Hint>
      <Answered text={run.answer} />
      {#if run.answer_copies?.length}
        <div class="row" style="margin-top:12px; flex-wrap:wrap; gap:8px">
          <span class="muted">Also put here, because the task asked for it:</span>
          {#each run.answer_copies as c}
            <Hint id="run.answer_copy">
              <button disabled={opening === c.id} onclick={() => openCopy(c)}>
                {c.kind === "note" ? "Note" : c.kind === "file" ? "File" : "Message"}: {c.label}
              </button>
            </Hint>
          {/each}
        </div>
      {/if}
    </div>
  {/if}

  {#if run.failure}
    <div class="err">
      <h3>It could not finish</h3>
      <!-- One line, then the one thing to do. This used to be three paragraphs
           whose headings were markdown that nothing rendered, so a person met
           "**What I was doing:**" in raw asterisks before reaching anything
           they could act on. -->
      <Hint id="run.failure">
        <div style="white-space:pre-wrap">{run.failure.plain_reason}</div>
      </Hint>
      {#if run.failure.fix}
        <div class="fix">{run.failure.fix}</div>
      {/if}
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
  {/if}
  {#if run.summary}
    <div class="card muted"><strong>What it did</strong><div style="margin-top:4px">{run.summary}</div></div>
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
  /* The thing to do, set apart from the thing that went wrong: a person
     scanning a failure is looking for this line. */
  .fix { margin-top: 8px; font-weight: 500; }

  .shot {
    display: block; max-width: 100%; margin-top: 8px;
    border: 1px solid var(--rule); border-radius: 6px;
  }
</style>
