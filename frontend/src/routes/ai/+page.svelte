<!--
  Which AI is doing the work.

  This screen exists because "it uses AI" is not an answer. Anyone should be
  able to open it and see, without reading anything, which model would run their
  task, whether it is on this machine or somebody else's, and what would happen
  if it were not there.

  It is also under a standing rule: it must never offer a choice that does
  nothing. A job Errand does not consult a model for says so, in place, rather
  than presenting a dropdown that is quietly ignored.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import {
    api,
    ApiError,
    type AiSetup,
    type KnownService,
    type ListedProvider,
    type Provider,
    type ScanResult,
  } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";

  let setup = $state<AiSetup | null>(null);
  let services = $state<KnownService[]>([]);
  let problem = $state<string | null>(null);
  let busy = $state(false);
  let found = $state<Provider[] | null>(null);
  let scan = $state<ScanResult | null>(null);
  let scanned = $state(false);
  let scanNetwork = $state(false);

  // Adding a service by name.
  let pick = $state("");
  let pickKey = $state("");
  let pickModel = $state("");

  // Adding something Errand has not heard of.
  let addLabel = $state(""), addUrl = $state(""), addModel = $state(""), addKey = $state("");
  let showCustom = $state(false);

  const ANTHROPIC = "anthropic";
  const chosen = $derived(services.find((s) => s.id === pick) ?? null);
  const pickedAnthropic = $derived(pick === ANTHROPIC);

  const ROLE_NAMES: Record<string, string> = {
    executor: "Doing the task",
    planner: "Writing down what it learned",
    fixer: "Working out why something failed",
    narrator: "Writing the message you get",
  };

  async function load() {
    try {
      setup = await api.ai();
      if (!services.length) services = await api.aiCatalogue();
      problem = null;
    } catch (e) { problem = e instanceof ApiError ? e.message : String(e); }
  }
  onMount(load);

  async function act(fn: () => Promise<unknown>) {
    busy = true;
    try { await fn(); await load(); }
    catch (e) { problem = e instanceof ApiError ? e.message : String(e); }
    finally { busy = false; }
  }

  // Checking one model is its own flow, not an `act`: the button says what it
  // is doing, the answer lands on the row it came from, and a failure is shown
  // there too rather than at the top of a page nobody is looking at.
  let checkingId = $state<string | null>(null);
  let checked = $state<Record<string, string>>({});

  function checkOutcome(before: string | null, after: string): string {
    const now = health(after).toLowerCase();
    if (before === after) return `Checked just now: still ${now}.`;
    const was = before ? health(before).toLowerCase() : "not checked";
    return `Checked just now: ${now} (was ${was}).`;
  }

  async function check(p: Provider) {
    checkingId = p.id;
    try {
      const r = await api.testProvider(p.id);
      checked[p.id] = checkOutcome(p.health, r.health);
      problem = null;
      await load();
    } catch (e) {
      checked[p.id] = `That check did not work: ${e instanceof ApiError ? e.message : String(e)}`;
    } finally {
      checkingId = null;
    }
  }

  // Every model nobody has asked yet, and that asking could tell us about.
  // Shown by name rather than counted, because "three not checked" tells you
  // nothing about whether the one you care about is among them.
  const unchecked = $derived(
    (setup?.providers ?? []).filter((p) => p.tools === "unknown" && !p.cannot_carry_out_because),
  );

  // Checking asks each one a single question, one after another, so a machine
  // that is asleep costs its own wait and not everybody else's.
  async function checkTheUnchecked() {
    for (const p of unchecked) await check(p);
  }

  /**
   * What to say after a model's name in the list of who could do the task.
   *
   * Nothing at all for one that can, because a list where every line carries a
   * note is a list nobody reads. The reason itself sits under the dropdown.
   */
  function taskNote(p: ListedProvider): string {
    if (p.tools === "no") return " (cannot use tools)";
    if (p.cannot_carry_out_because) return " (cannot do this job)";
    if (p.tools === "unknown") return " (not checked yet)";
    return "";
  }

  const toolsDot = (p: ListedProvider) =>
    p.tools === "yes" ? "ok" : p.cannot_carry_out_because ? "warn" : "";

  /**
   * Which model this endpoint is using, said next to its name.
   *
   * The complaint this answers: the row read "Claude (command line tool)" and
   * stopped, so somebody who chooses between Opus, Sonnet and Haiku everywhere
   * else could not find out which one Errand had picked. The command line tool
   * has one per job, so its answer is a sentence rather than a name.
   */
  const modelLine = (p: ListedProvider) => p.models_in_use ?? p.model ?? "";

  async function runScan() {
    busy = true;
    try {
      scan = await api.discoverProviders(scanNetwork);
      found = scan.found;
      scanned = true;
      problem = null;
    } catch (e) { problem = e instanceof ApiError ? e.message : String(e); }
    finally { busy = false; }
  }

  async function addService() {
    if (pickedAnthropic) {
      await act(() => api.saveAnthropicKey(pickKey.trim()));
      pick = ""; pickKey = ""; pickModel = "";
      return;
    }
    await act(() => api.saveProvider({
      known: pick,
      key: pickKey.trim() || undefined,
      model: pickModel.trim() || undefined,
      enabled: true,
    }));
    pick = ""; pickKey = ""; pickModel = "";
  }

  async function addFound(p: Provider) {
    await act(() => api.saveProvider({
      kind: "openai_compat", label: p.label,
      base_url: p.base_url ?? "", model: p.model ?? "", enabled: true,
    }));
    found = found?.filter((f) => f.id !== p.id) ?? null;
  }

  async function addTyped() {
    await act(() => api.saveProvider({
      kind: "openai_compat",
      label: addLabel.trim() || "My model",
      base_url: addUrl.trim(),
      model: addModel.trim(),
      key: addKey.trim() || undefined,
      enabled: true,
    }));
    addLabel = ""; addUrl = ""; addModel = ""; addKey = "";
  }

  const toggle = (p: Provider) =>
    act(() => api.saveProvider({
      id: p.id, kind: p.kind, label: p.label,
      base_url: p.base_url ?? undefined, model: p.model ?? undefined,
      enabled: !p.enabled,
    }));

  const dot = (h: string | null) => (h === "ok" ? "ok" : h === "empty" ? "warn" : h ? "bad" : "");
  const health = (h: string | null) =>
    ({ ok: "Working", missing: "Needs a key", unreachable: "Not answering",
       empty: "No model loaded", unknown: "Unrecognised" }[h ?? ""] ?? "Not checked yet");
  const where = (p: Provider) =>
    p.kind === "claude_cli" ? "Runs here, sends your task text to Anthropic"
    : p.kind === "anthropic_api" ? "Sends your task text to Anthropic"
    : p.base_url && /127\.0\.0\.1|localhost/.test(p.base_url) ? "Stays on this machine"
    : p.base_url && /^http:\/\/(10|192\.168|172)\./.test(p.base_url) ? "Stays on your network"
    : "Sends your task text to this service";
</script>

<h1>AI</h1>
<p class="lede">
  Errand has no AI of its own. Every job below is done by a model you provide, and this is where
  you see which one, and change it.
</p>

{#if problem}<div class="err"><h3>Something went wrong</h3><div>{problem}</div></div>{/if}

{#if setup}
  <h2>What is doing each job</h2>
  <div class="roles">
    {#each setup.roles as r}
      <div class="card role" class:idle={!r.in_use}>
        <Hint id="ai.role">
          <div class="rname">
            {ROLE_NAMES[r.role] ?? r.role}
            {#if !r.in_use}<span class="pill">Not used yet</span>{/if}
          </div>
        </Hint>
        <div class="muted">{r.explains}</div>

        {#if !r.in_use}
          <div class="warnbox">{r.not_used_because}</div>
        {:else if r.chosen_problem}
          <!-- Picked before Errand knew what it could do. Saying so is better
               than quietly using something else and letting the person believe
               their choice stuck. -->
          <div class="warnbox">
            {r.chosen_problem}
            {#if r.using}Errand is using {r.using.label} instead.{/if}
          </div>
        {:else if r.using}
          <div class="using">
            <span class="pill ok">{r.using.label}</span>
            <!-- The Claude command line tool answers to three models, so naming
                 the provider does not answer "which one is it". Where there is
                 a choice it is made here; where there is not, the model is
                 still said rather than left to be guessed at. -->
            {#if r.using.can_choose_model}
              <Hint id="ai.model">
                <select
                  class="model"
                  disabled={busy}
                  value={r.using.model}
                  onchange={(e) => act(() => api.setRoleModel(r.role, (e.currentTarget as HTMLSelectElement).value))}
                >
                  {#each setup.claude_models as m}
                    <option value={m.alias}>{m.name}</option>
                  {/each}
                </select>
              </Hint>
            {:else}
              <span class="mono">{r.using.model}</span>
            {/if}
            <Hint id="ai.local">
              <span class="pill {r.using.local ? 'ok' : ''}">
                {r.using.local ? "Stays on your machine" : "Leaves your machine"}
              </span>
            </Hint>
          </div>
          {#if r.using.model_says}
            <div class="muted">{r.using.model_says}</div>
          {/if}
          {#if r.fallbacks.length}
            <div class="muted">If that is unavailable: {r.fallbacks.join(", ")}</div>
          {/if}
        {:else}
          <div class="warnbox">{r.problem}</div>
        {/if}

        {#if r.in_use}
          <Hint id="ai.pick">
            <select
              disabled={busy}
              value={r.chosen ?? ""}
              onchange={(e) => act(() => api.bindRole(r.role, (e.currentTarget as HTMLSelectElement).value || null))}
            >
              <option value="">No preference: use whatever works</option>
              {#each setup.providers as p}
                <option value={p.id} disabled={r.needs_agentic && !!p.cannot_carry_out_because}>
                  {p.label}{r.needs_agentic ? taskNote(p) : ""}
                </option>
              {/each}
            </select>
          </Hint>

          {#if r.needs_agentic}
            <!-- What is actually known, rather than a blanket refusal. A model
                 is only called incapable where Errand has asked it and found
                 out; everything else says plainly that nobody has looked. -->
            {#each setup.providers.filter((p) => p.cannot_carry_out_because) as p}
              <div class="muted">{p.cannot_carry_out_because}</div>
            {/each}

            {#if unchecked.length}
              <div class="muted">
                Not checked yet: {unchecked.map((p) => p.label).join(", ")}. Errand has not asked
                {unchecked.length === 1 ? "it" : "them"} to use a tool, so it does not know. You can
                pick {unchecked.length === 1 ? "it" : "one"} anyway.
              </div>
              <Hint id="ai.check_tools">
                <button disabled={busy || checkingId !== null} onclick={checkTheUnchecked}>
                  {checkingId !== null ? "Asking…" : "Check which can carry out a task"}
                </button>
              </Hint>
            {/if}

            <div class="muted">
              Any model that can use tools can do this job. One that only answers questions cannot
              drive a browser. Being able to is not the same as being good at it: a small model
              will misread a page and give up half way through, so use the best one you have.
            </div>

            <!-- Said from this end as well as from the task's, because whoever
                 is looking at one of the two screens is the one who needs to
                 know that the other exists. -->
            <div class="muted">
              This is the default for every task. A single task can name a model of its own on its
              page, and that choice wins for that task: worth doing where a task reads your mail
              or anything else private, since the model doing the job is what reads it.
            </div>
          {/if}
        {/if}
      </div>
    {/each}
  </div>

  <h2>Models Errand can reach</h2>
  {#each setup.providers as p}
    <div class="card row">
      <div class="grow">
        <div class="rname">
          {p.label}
          <span class="pill {dot(p.health)}">{health(p.health)}</span>
          <Hint id="ai.tools">
            <span class="pill {toolsDot(p)}">{p.tools_says}</span>
          </Hint>
          {#if !p.enabled}<span class="pill">Switched off</span>{/if}
        </div>
        <div class="muted">{where(p)}{p.base_url ? ` · ${p.base_url}` : ""}{modelLine(p) ? ` · ${modelLine(p)}` : ""}</div>
        {#if p.health_detail}<div class="muted detail">{p.health_detail}</div>{/if}
        {#if checked[p.id]}<div class="checked">{checked[p.id]}</div>{/if}
      </div>
      <div class="actions">
        <Hint id="ai.test">
          <button disabled={busy || checkingId === p.id} onclick={() => check(p)}>
            {checkingId === p.id ? "Checking, this can take a minute or two…" : "Check"}
          </button>
        </Hint>
        <Hint id="ai.enable">
          <button disabled={busy} onclick={() => toggle(p)}>{p.enabled ? "Switch off" : "Switch on"}</button>
        </Hint>
        <Hint id="ai.remove">
          <button disabled={busy} onclick={() => act(() => api.removeProvider(p.id))}>Remove</button>
        </Hint>
      </div>
    </div>
  {/each}

  <h2>Add a service</h2>
  <div class="card">
    <div class="muted" style="margin-bottom:10px">
      Errand knows the addresses of these already. Pick one, paste a key, and it works. Your key
      goes into your macOS keychain and never into Errand's database, its logs, or this window.
    </div>
    <div class="form">
      <label for="ai-service">Service</label>
      <Hint id="ai.service">
        <select id="ai-service" bind:value={pick} disabled={busy}>
          <option value="">Choose one…</option>
          <option value={ANTHROPIC}>Anthropic (your own key)</option>
          {#each services as s}
            <option value={s.id}>{s.name}</option>
          {/each}
        </select>
      </Hint>

      {#if pickedAnthropic}
        <div class="muted">
          Optional. Errand already uses the Claude command line tool, which is signed in. A key
          here bills your own Anthropic account instead.
          · <a href="https://console.anthropic.com/settings/keys" target="_blank" rel="noreferrer noopener" data-hint-exempt="opens Anthropic's own page for getting a key">Get a key</a>
        </div>
        <label for="ai-anthropic-key">Key</label>
        <input id="ai-anthropic-key" type="password" bind:value={pickKey} autocomplete="off" placeholder="sk-ant-…" />
        <Hint id="ai.key">
          <button disabled={busy || !pickKey.trim()} onclick={addService}>Add Anthropic</button>
        </Hint>
      {:else if chosen}
        <div class="muted">
          {chosen.base_url}
          {#if chosen.needs_key}
            · <a href={chosen.keys_url} target="_blank" rel="noreferrer noopener" data-hint-exempt="opens that service's own page for getting a key">Get a key</a>
          {/if}
        </div>

        {#if chosen.needs_key}
          <label for="ai-svc-key">Key</label>
          <input id="ai-svc-key" type="password" bind:value={pickKey} autocomplete="off"
                 placeholder={chosen.key_prefix ? `${chosen.key_prefix}…` : "paste your key"} />
        {/if}

        <label for="ai-svc-model">Model</label>
        <input id="ai-svc-model" bind:value={pickModel} placeholder={chosen.example_model || "leave blank for the default"} />

        <Hint id="ai.add_service">
          <button disabled={busy || (chosen.needs_key && !pickKey.trim())} onclick={addService}>
            Add {chosen.name}
          </button>
        </Hint>
      {/if}
    </div>
  </div>

  <h2>Models on your own machines</h2>
  <div class="card">
    <div class="muted" style="margin-bottom:10px">
      Ollama, LM Studio, vLLM, llama.cpp, GPT4All and Open WebUI all speak the same language, so
      Errand can use any of them, for any of the four jobs. A model here never sends your task to
      anyone. If it can use tools, it can carry out the task itself: Errand hands it the browser
      and runs the loop. Adding one asks it a single question to find out.
    </div>

    <Hint id="ai.scan_network">
      <label class="check">
        <input type="checkbox" bind:checked={scanNetwork} />
        Look on my network too, not just this machine
      </label>
    </Hint>
    <div class="muted" style="margin:4px 0 10px">
      {scanNetwork
        ? "Errand will try every address on the network this machine is on. That takes a few seconds. Do not do this on a network that is not yours."
        : "Only this machine. Nothing is sent to your network."}
    </div>

    <Hint id="ai.scan">
      <button disabled={busy} onclick={runScan}>{busy ? "Looking…" : "Look for models"}</button>
    </Hint>

    {#if scan?.blocked}
      <div class="warnbox" style="margin-top:10px">{scan.blocked}</div>
    {/if}

    {#if scanned && scan && !scan.blocked}
      <!-- Said out loud, so an empty result reads as "it looked and there was
           nothing" rather than "it probably did not work". -->
      <div class="muted" style="margin-top:10px">
        Tried {scan.ports} ports on {scan.addresses} address{scan.addresses === 1 ? "" : "es"}:
        {scan.found.length} usable{scan.also_seen.length ? `, ${scan.also_seen.length} that answered but cannot be used as they are` : ""}.
      </div>
    {/if}

    {#if scanned}
      {#if found && found.length}
        {#each found as f}
          <div class="row found">
            <div class="grow">
              <div>{f.label}</div>
              <div class="muted">{f.base_url}</div>
              {#if f.health_detail}<div class="muted detail">{f.health_detail}</div>{/if}
            </div>
            <Hint id="ai.add_found">
              <button disabled={busy} onclick={() => addFound(f)}>Use this</button>
            </Hint>
          </div>
        {/each}
      {:else}
        <div class="muted" style="margin-top:10px">
          Nothing usable answered. If your model runs somewhere Errand did not look, such as
          another machine, a port of its own, or behind a name rather than a number, add it by
          address below.
        </div>
      {/if}

      {#if scan?.also_seen.length}
        <div class="alsobox">
          <strong>Also answered, but not usable as they are</strong>
          {#each scan.also_seen as a}
            <div class="muted">{a.why} <span class="mono">{a.url}</span></div>
          {/each}
        </div>
      {/if}

      <div class="muted" style="margin-top:10px">
        A server reached by a name rather than a number, such as anything behind a reverse proxy
        that routes on the hostname, cannot be found by looking at addresses. Add those by address.
      </div>
    {/if}

    <Hint id="ai.custom">
      <button class="linky" onclick={() => (showCustom = !showCustom)}>
        {showCustom ? "Never mind" : "Add one by address instead"}
      </button>
    </Hint>

    {#if showCustom}
      <div class="form">
        <label for="ai-url">Address</label>
        <input id="ai-url" bind:value={addUrl} placeholder="http://the-other-machine.local:11434" />
        <div class="muted">
          A machine name usually works better than a number, because a number can change: try the
          computer's name followed by .local, as in http://mini.local:11434. On your own machine or
          your own network you can leave off the /v1 and Errand will add it.
        </div>
        <label for="ai-model">Model name</label>
        <input id="ai-model" bind:value={addModel} placeholder="llama3.1" />
        <label for="ai-label">What to call it</label>
        <input id="ai-label" bind:value={addLabel} placeholder="The mini PC" />
        <label for="ai-custom-key">Key, if it needs one</label>
        <input id="ai-custom-key" type="password" bind:value={addKey} autocomplete="off" placeholder="usually blank" />
        <Hint id="ai.add">
          <button disabled={busy || !addUrl.trim()} onclick={addTyped}>Add it</button>
        </Hint>
      </div>
    {/if}
  </div>

  <h2>Keep everything on this machine</h2>
  <div class="card">
    <Hint id="ai.local_only">
      <button disabled={busy} onclick={() => act(() => api.setLocalOnly(!setup!.local_only))}>
        {setup.local_only ? "On: nothing leaves your machines" : "Off: Errand may use a service"}
      </button>
    </Hint>
    <div class="muted" style="margin-top:8px">
      With this on, Errand refuses to send anything to a model it does not reach on your own
      machine or your own network. Tasks that need a browser keep working, as long as one of your
      own models can use tools. If none can, they stop rather than quietly going to a service.
    </div>
  </div>
{:else if !problem}
  <div class="card muted">Asking the background service what it is set up with…</div>
{/if}

<style>
  .lede { color: var(--ink-soft); max-width: 62ch; margin: 0 0 18px; }
  .roles { display: grid; gap: 10px; grid-template-columns: repeat(auto-fit, minmax(280px, 1fr)); }
  .role { display: flex; flex-direction: column; gap: 8px; }
  .role.idle { opacity: 0.72; }
  .rname { font-weight: 600; display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  .using { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  /* Sized to the name in it, so the model chooser sits on the line beside the
     provider rather than pushing everything else onto its own row. */
  .using select.model { width: auto; font-size: 12.5px; padding: 2px 6px; }
  .mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; color: var(--ink-soft); }
  .detail { word-break: break-word; }
  /* The answer to "Check", on the row that was asked. Stays visible until the
     next check, so an unchanged verdict is a visible answer rather than a
     click that looked like it did nothing. */
  .checked { margin-top: 4px; font-size: 12.5px; color: var(--ink-soft); }
  .alsobox {
    margin-top: 10px; padding: 9px 11px; border-radius: 6px;
    background: var(--warn-bg); color: var(--warn); font-size: 12.5px;
    display: flex; flex-direction: column; gap: 3px;
  }
  .alsobox .mono { opacity: 0.85; }
  .warnbox {
    background: var(--warn-bg); color: var(--warn);
    padding: 9px 11px; border-radius: 6px; font-size: 12.5px; max-width: 520px;
  }
  .row { display: flex; align-items: flex-start; gap: 12px; }
  .row.found { border-top: 1px solid var(--line); padding-top: 10px; margin-top: 10px; }
  .grow { flex: 1; min-width: 0; }
  .actions { display: flex; gap: 6px; flex-shrink: 0; }
  .warnbox {
    background: var(--warn-bg); color: var(--warn);
    padding: 8px 10px; border-radius: 6px; font-size: 12.5px;
  }
  .form { display: grid; gap: 6px; margin-top: 12px; max-width: 460px; }
  .form label { font-size: 12px; color: var(--ink-faint); }
  .check { display: flex; align-items: center; gap: 8px; font-size: 13px; cursor: pointer; }
  .check input { width: auto; }
  select { width: 100%; }
  .linky {
    background: none; border: none; padding: 6px 0; color: var(--accent);
    cursor: pointer; font-size: 12.5px; text-align: left;
  }
</style>
