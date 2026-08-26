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

  let {
    sites = $bindable(),
    /** Addresses found in what the person wrote, offered rather than assumed. */
    suggestions = [],
    /**
     * True while a task is still being written. An empty list is then simply
     * unfinished, not broken, and saying "it cannot open anything" reads as
     * telling somebody off for a box they are in the middle of filling.
     */
    creating = false,
  }: { sites: string[]; suggestions?: string[]; creating?: boolean } = $props();

  const offered = $derived(suggestions.filter((s) => !sites.includes(s)));

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
  {#if sites.length === 0 && !creating}
    <div class="warnbox">
      This task has no sites yet, so it cannot open anything. Add the site it needs to visit.
    </div>
  {/if}

  {#if offered.length}
    <div class="offer">
      <span class="muted">Found in your description:</span>
      {#each offered as o}
        <Hint id="task.suggested_site">
          <button type="button" class="chip" onclick={() => (sites = [...sites, o])}>
            + {o}
          </button>
        </Hint>
      {/each}
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
    {#if sites.length === 0 && creating && offered.length === 0}
      Errand needs the address of the site, such as example.com, because naming a company is not
      enough to know which site is really theirs. It will only ever open the ones you list here.
    {:else}
      Subdomains are included: booking.example.com is covered by example.com. Anything not listed
      is refused, including a redirect from a site that is listed.
    {/if}
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
  .offer { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  .chip {
    background: var(--surface-2); color: var(--accent);
    border: 1px solid var(--rule); border-radius: 999px;
    padding: 2px 9px; font-size: 12px; cursor: pointer;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  .chip:hover { border-color: var(--accent); }
  .warnbox {
    background: var(--warn-bg); color: var(--warn);
    padding: 8px 10px; border-radius: 6px; font-size: 12.5px;
  }
  .sr {
    position: absolute; width: 1px; height: 1px; overflow: hidden;
    clip: rect(0 0 0 0); white-space: nowrap;
  }
</style>
