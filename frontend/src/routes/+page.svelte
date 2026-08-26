<script lang="ts">
  import { onMount } from "svelte";
  import { api, when, type Task, ApiError } from "$lib/api";
  import { headlineFor, hintFor, subline, isLive } from "$lib/taskState";
  import Hint from "$lib/components/Hint.svelte";
  import SitesEditor from "$lib/components/SitesEditor.svelte";
  import Trouble from "$lib/components/Trouble.svelte";
  import { reconnecting } from "$lib/reconnect.svelte";
  import { suggestFromText } from "$lib/sites";

  let tasks = $state<Task[]>([]);
  let loading = $state(true);
  let problem = $state<string | null>(null);
  let creating = $state(false);
  let newSites = $state<string[]>([]);
  let name = $state("");
  let description = $state("");
  // What Errand worked out for itself, shown once, right after creating.
  let setUp = $state<{ what: string; because: string }[]>([]);
  let setUpFor = $state<string | null>(null);

  const conn = reconnecting(
    async () => {
      tasks = await api.tasks();
    },
    (p) => {
      problem = p;
      loading = false;
    },
  );
  const load = conn.run;
  onMount(() => {
    conn.run();
    return conn.stop;
  });

  let working = $state(false);

  async function create() {
    working = true;
    try {
      const t = await api.createTask(name.trim(), description.trim(), undefined, newSites);
      name = ""; description = ""; newSites = []; creating = false;
      // Shown here rather than on the task, because this is the moment a
      // person wants to know what was decided for them, and because most of
      // the time the honest answer is a line or two and then they move on.
      setUp = t.set_up ?? [];
      setUpFor = t.name;
      // Start the job. Making a task and then asking somebody to press a
      // second button is the ceremony this is meant to be free of: they have
      // just described a job, and the answer is what they came for.
      try { await api.run(t.id); } catch { /* the task page will say why */ }
      await load();
      if (setUp.length === 0) location.href = `/task/${t.id}`;
    } catch (e) {
      problem = e instanceof ApiError ? e.message : String(e);
    } finally {
      working = false;
    }
  }

</script>

<h1>Tasks</h1>
<p class="deck">Jobs you have handed to Errand. Describe one and it works out how to do it, does it, and shows you what came back.</p>

{#if problem}
  <Trouble {problem} retrying={conn.retrying} onRetry={() => conn.run(true)} />
{/if}

{#if !creating}
  <Hint id="task.new">
    <button class="primary" onclick={() => (creating = true)}>New task</button>
  </Hint>
{:else}
  <div class="card">
    <label for="n">What should it be called? <span class="muted">Optional</span></label>
    <input id="n" bind:value={name} placeholder="Left blank, Errand names it from the job" />
    <label for="d">Describe the job the way you would to a person</label>
    <textarea id="d" rows="4" bind:value={description}
      placeholder="Go to the club website, sign in, and book the Wednesday 19:00 court. Tell me the confirmation number."></textarea>
    <p class="muted">This description is what the agent actually reads, so be as plain as you would be with a person.</p>

    <label for="sites-block">Which websites may it open?</label>
    <div id="sites-block">
      <SitesEditor bind:sites={newSites} suggestions={suggestFromText(description)} creating />
    </div>
    <p class="muted">
      Leave this empty and Errand works out which sites the job needs. Name one yourself and it
      uses yours and adds nothing, because the first site decides which saved logins the task
      gets. You can change it either way later.
    </p>
    <div class="row" style="margin-top:12px">
      <Hint id="task.create">
        <button class="primary" disabled={working || !description.trim()} onclick={create}>
          {working ? "Setting it up…" : "Create and do it"}
        </button>
      </Hint>
      <button onclick={() => (creating = false)} data-hint-exempt="cancels the form above, changes nothing">Cancel</button>
    </div>
  </div>
{/if}

{#if setUp.length}
  <div class="card" style="margin-top:14px">
    <strong>{setUpFor} is running now. Errand set this up for you:</strong>
    <ul class="setup">
      {#each setUp as n}
        <li>{n.what} <span class="muted">because {n.because}</span></li>
      {/each}
    </ul>
    <p class="muted" style="margin:8px 0 0">
      Change any of it with the gear on the task. It cannot sign in anywhere, message anybody or
      spend anything: those are yours to switch on.
    </p>
  </div>
{/if}

{#if loading}
  <p class="muted" style="margin-top:20px">Loading…</p>
{:else if problem}
  <!-- Nothing is said about the list here on purpose. "Nothing here yet" under
       a failed fetch is a lie with a red banner above it, and it is the lie
       that reads first. -->
{:else if tasks.length === 0}
  <div class="card" style="margin-top:20px">
    <strong>Nothing here yet.</strong>
    <p class="muted" style="margin:6px 0 0">
      Describe a job in your own words. Errand works out what it needs, does it, and shows you
      what came back. Once you have seen it work you can put it on a schedule.
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
              <Hint id={hintFor(t, t.last_run)}>
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
