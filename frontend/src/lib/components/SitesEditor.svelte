<!--
  The websites a task may open.

  This is the setting that decides whether a task can do anything at all, and
  the one most likely to be typed slightly wrong. A site is stored as a bare
  host, subdomains are included automatically, and there are no wildcards, so
  "*.example.com" looks reasonable, saves happily, and matches nothing. The
  daemon refuses those with an explanation; this screen shows the explanation
  next to the box rather than as a failure ten seconds later.

  Order matters and is not cosmetic: the first site decides which browser
  profile the task uses, which is where its saved logins live.
-->
<script lang="ts">
  import Hint from "$lib/components/Hint.svelte";

  let { sites = $bindable() }: { sites: string[] } = $props();

  let entry = $state("");
  let localProblem = $state<string | null>(null);

  function add() {
    const raw = entry.trim();
    if (!raw) return;
    // Only the obvious mistakes are caught here. The daemon is the authority,
    // because its rule is the one the browser actually enforces.
    if (raw.includes("*")) {
      localProblem =
        "Subdomains are already included, so there is no need for a *. Enter the plain site, " +
        "such as example.com.";
      return;
    }
    const cleaned = raw.replace(/^https?:\/\//i, "").replace(/\/.*$/, "").toLowerCase();
    if (!cleaned.includes(".")) {
      localProblem = `"${cleaned}" is not a whole site. Enter something like example.com.`;
      return;
    }
    if (sites.includes(cleaned)) {
      localProblem = `${cleaned} is already on the list.`;
      return;
    }
    sites = [...sites, cleaned];
    entry = "";
    localProblem = null;
  }

  const remove = (s: string) => (sites = sites.filter((x) => x !== s));

  function moveFirst(s: string) {
    sites = [s, ...sites.filter((x) => x !== s)];
  }
</script>

<div class="sites">
  {#if sites.length === 0}
    <div class="warnbox">
      This task has no sites yet, so it cannot open anything. Add the site it needs to visit.
    </div>
  {/if}

  <ul>
    {#each sites as s, i}
      <li>
        <span class="host">{s}</span>
        {#if i === 0 && sites.length > 1}
          <Hint id="task.first_site"><span class="pill">Signs in here</span></Hint>
        {:else if sites.length > 1}
          <Hint id="task.first_site">
            <button type="button" class="linky" onclick={() => moveFirst(s)}>Make this the main one</button>
          </Hint>
        {/if}
        <Hint id="task.remove_site">
          <button type="button" class="x" onclick={() => remove(s)} aria-label={`Remove ${s}`}>Remove</button>
        </Hint>
      </li>
    {/each}
  </ul>

  <div class="add">
    <label for="site-entry" class="sr">Site to add</label>
    <input id="site-entry"
      bind:value={entry}
      placeholder="example.com"
      spellcheck="false"
      autocapitalize="off"
      onkeydown={(e) => { if (e.key === "Enter") { e.preventDefault(); add(); } }}
    />
    <Hint id="task.add_site">
      <button type="button" disabled={!entry.trim()} onclick={add}>Add site</button>
    </Hint>
  </div>

  {#if localProblem}<div class="warnbox">{localProblem}</div>{/if}

  <div class="muted">
    Subdomains are included: booking.example.com is covered by example.com. Anything not listed is
    refused, including a redirect from a site that is listed.
  </div>
</div>

<style>
  .sites { display: flex; flex-direction: column; gap: 8px; max-width: 520px; }
  ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 4px; }
  li { display: flex; align-items: center; gap: 8px; }
  .host { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12.5px; }
  .add { display: flex; gap: 6px; }
  .add input { flex: 1; }
  .x, .linky {
    background: none; border: none; padding: 0; color: var(--accent);
    cursor: pointer; font-size: 12px;
  }
  .warnbox {
    background: var(--warn-bg); color: var(--warn);
    padding: 8px 10px; border-radius: 6px; font-size: 12.5px;
  }
  .sr {
    position: absolute; width: 1px; height: 1px; overflow: hidden;
    clip: rect(0 0 0 0); white-space: nowrap;
  }
</style>
