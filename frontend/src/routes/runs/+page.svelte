<script lang="ts">
  import { onMount } from "svelte";
  import Trouble from "$lib/components/Trouble.svelte";
  import { reconnecting } from "$lib/reconnect.svelte";
  import { api, statusLabel, when, ApiError, type Run } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";

  let runs = $state<Run[]>([]);
  let problem = $state<string | null>(null);
  const conn = reconnecting(
    async () => {
      runs = await api.runs();
    },
    (p) => (problem = p),
  );
  onMount(() => {
    conn.run();
    return conn.stop;
  });
</script>

<h1>History</h1>
<p class="deck">Everything Errand has done, most recent first.</p>
{#if problem}
  <Trouble {problem} retrying={conn.retrying} onRetry={() => conn.run(true)} />
{/if}
{#if runs.length === 0 && !problem}
  <p class="muted">Nothing has run yet.</p>
{/if}
{#each runs as r}
  <a class="plain" href={`/run/${r.id}`} data-hint-exempt="opens the run shown in this card">
    <div class="card">
      <div class="row spread">
        <div>
          <Hint id="run.status">
            <span class="pill {r.status === 'succeeded' ? 'ok' : r.status === 'failed' ? 'bad' : ''}">
              {statusLabel(r.status)}
            </span>
          </Hint>
          <div class="muted" style="margin-top:6px">
            {(r.summary ?? r.failure?.plain_reason ?? "").split("\n")[0].slice(0, 120)}
          </div>
        </div>
        <span class="muted">{when(r.created_at)}</span>
      </div>
    </div>
  </a>
{/each}
