<script lang="ts">
  import { onMount } from "svelte";
  import { api, when, type Task, ApiError } from "$lib/api";
  import { headlineFor, subline, isLive } from "$lib/taskState";
  import Hint from "$lib/components/Hint.svelte";
  import SitesEditor from "$lib/components/SitesEditor.svelte";

  let tasks = $state<Task[]>([]);
  let loading = $state(true);
  let problem = $state<string | null>(null);
  let creating = $state(false);
  let newSites = $state<string[]>([]);
  let name = $state("");
  let description = $state("");

  async function load() {
    try {
      tasks = await api.tasks();
      problem = null;
    } catch (e) {
      problem = e instanceof ApiError ? e.message : String(e);
    } finally {
      loading = false;
    }
  }
  onMount(load);

  async function create() {
    try {
      const t = await api.createTask(name.trim(), description.trim(), undefined, newSites);
      name = ""; description = ""; creating = false;
      location.href = `/task/${t.id}`;
    } catch (e) {
      problem = e instanceof ApiError ? e.message : String(e);
    }
  }

</script>

<h1>Tasks</h1>
<p class="deck">Jobs you have handed to Errand. Each one runs on its own once you have taught it.</p>

{#if problem}
  <div class="err"><h3>Something is not right</h3><div>{problem}</div></div>
{/if}

{#if !creating}
  <Hint id="task.new">
    <button class="primary" onclick={() => (creating = true)}>New task</button>
  </Hint>
{:else}
  <div class="card">
    <label for="n">What should it be called?</label>
    <input id="n" bind:value={name} placeholder="Book the Wednesday court" />
    <label for="d">Describe the job the way you would to a person</label>
    <textarea id="d" rows="4" bind:value={description}
      placeholder="Go to the club website, sign in, and book the Wednesday 19:00 court. Tell me the confirmation number."></textarea>
    <p class="muted">This description is what the agent actually reads, so be as plain as you would be with a person.</p>

    <label for="sites-block">Which websites may it open?</label>
    <div id="sites-block">
      <SitesEditor bind:sites={newSites} />
    </div>
    <p class="muted">
      A task with no sites cannot browse at all, so this is worth getting right now rather than
      after it has tried once. You can change it later.
    </p>
    <div class="row" style="margin-top:12px">
      <Hint id="task.create">
        <button class="primary" disabled={!name.trim() || !description.trim()} onclick={create}>Create</button>
      </Hint>
      <button onclick={() => (creating = false)} data-hint-exempt="cancels the form above, changes nothing">Cancel</button>
    </div>
  </div>
{/if}

{#if loading}
  <p class="muted" style="margin-top:20px">Loading…</p>
{:else if tasks.length === 0}
  <div class="card" style="margin-top:20px">
    <strong>Nothing here yet.</strong>
    <p class="muted" style="margin:6px 0 0">
      Create a task, teach it once while you watch, and approve what it learned. After that it runs on its own.
    </p>
  </div>
{:else}
  <div style="margin-top:20px">
    {#each tasks as t}
      {@const h = headlineFor(t, t.last_run)}
      {@const sub = subline(t, t.last_run)}
      <a class="plain" href={`/task/${t.id}`} data-hint-exempt="opens the task shown in this card">
        <div class="card">
          <div class="row spread">
            <div>
              <strong>{t.emoji ?? ""} {t.name}</strong>
              <div class="muted">{t.description.slice(0, 96)}{t.description.length > 96 ? "…" : ""}</div>
              {#if sub}
                <div class="muted said" class:bad={t.last_run?.status === "failed"}>{sub}</div>
              {/if}
            </div>
            <div class="row">
              {#if t.next_run_at}
                <Hint id="task.next_run">
                  <span class="pill" title={t.next_run_at}>runs {when(t.next_run_at)}</span>
                </Hint>
              {/if}
              <Hint id="task.status">
                <span class="pill {h.cls}">
                  {#if isLive(t.last_run)}<span class="live"></span>{/if}{h.text}
                </span>
              </Hint>
            </div>
          </div>
          {#if t.auto_paused}
            <div class="muted" style="margin-top:8px; color: var(--bad)">
              Errand paused this itself: {t.paused_reason ?? "it needs you"}
            </div>
          {/if}
        </div>
      </a>
    {/each}
  </div>
{/if}
