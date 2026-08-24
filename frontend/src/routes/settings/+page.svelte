<script lang="ts">
  import { onMount } from "svelte";
  import { api, ApiError, type ChannelHealth, type Health } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";

  let health = $state<Health | null>(null);
  let channels = $state<ChannelHealth[]>([]);
  let notes = $state<Record<string, string>>({});
  let creds = $state<any[]>([]);
  let problem = $state<string | null>(null);
  let busy = $state(false);

  // Adding a login.
  let label = $state(""), domain = $state(""), username = $state(""), secret = $state("");

  async function load() {
    try {
      health = await api.health();
      const c = await api.channels();
      channels = c.channels; notes = c.notes;
      creds = await api.credentials();
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

  async function addCred() {
    await act(() => api.addCredential(label.trim(), domain.trim(), username.trim(), secret));
    label = ""; domain = ""; username = ""; secret = "";
  }

  const cls = (s: string) => (s === "ok" ? "ok" : s === "not_configured" ? "" : s === "needs_user" ? "warn" : "bad");
</script>

<h1>Settings</h1>
{#if problem}<div class="err"><h3>Something went wrong</h3><div>{problem}</div></div>{/if}

<h2>The background service</h2>
<div class="card">
  <Hint id="settings.daemon">
    <span class="pill {health ? 'ok' : 'bad'}">{health ? "Running" : "Not answering"}</span>
  </Hint>
  {#if health}
    <div class="muted" style="margin-top:8px">
      Version {health.version} · {health.busy_runs} run(s) in progress · keychain {health.keychain}
    </div>
    {#if health.keychain !== "ok"}
      <div class="muted" style="color:var(--warn); margin-top:6px">
        The keychain is not answering normally, so saved logins will not work until that is fixed.
      </div>
    {/if}
  {/if}
</div>

<h2>How Errand reaches you</h2>
{#each channels as c}
  <div class="card">
    <div class="row spread">
      <div>
        <Hint id={c.channel === "telegram" ? "channel.telegram" : c.channel === "whatsapp" ? "channel.whatsapp" : "channel.apple"}>
          <strong>{c.channel.replace("_", " ")}</strong>
        </Hint>
        <div class="muted" style="margin-top:4px">{c.detail}</div>
        {#if c.fix}<div class="muted" style="margin-top:4px; color:var(--warn)">{c.fix}</div>{/if}
      </div>
      <div class="row">
        {#if c.channel === "apple_mail" || c.channel === "imessage"}
          <Hint id="channel.apple">
            <button disabled={busy} onclick={() => act(() => api.enableChannel(c.channel))}>Enable</button>
          </Hint>
        {/if}
        <Hint id="channel.test">
          <button disabled={busy} onclick={() => act(() => api.testChannel(c.channel))}>Send test</button>
        </Hint>
        <span class="pill {cls(c.status)}">{c.status.replace("_", " ")}</span>
      </div>
    </div>
    {#if c.channel === "whatsapp" && notes.whatsapp}
      <div class="muted" style="margin-top:8px">{notes.whatsapp}</div>
    {/if}
  </div>
{/each}

<h2>Letting other programs in</h2>
<div class="card">
  <Hint id="settings.token"><span>Access key</span></Hint>
  <p class="muted" style="margin:6px 0 0">
    Another program, such as KinAI, needs a key to talk to Errand. Create one from a terminal with
    <code>errandd token --new</code>, and give each program only the permissions it needs. Errand
    keeps a scrambled copy only, so it can never show a key back to you.
  </p>
</div>
<div class="card">
  <Hint id="settings.quiet"><span>Quiet hours</span></Hint>
  <p class="muted" style="margin:6px 0 0">
    Messages to other people, and routine good news, wait until the quiet period ends. Something
    that failed still reaches you straight away.
  </p>
</div>

<h2>Logins</h2>
<Hint id="cred.what"><span class="muted">Stored in your macOS keychain, never in Errand's own files.</span></Hint>
{#each creds as c}
  <div class="card">
    <div class="row spread">
      <div>
        <strong>{c.label}</strong>
        <Hint id="cred.domain"><span class="muted"> · only on {c.domain}</span></Hint>
        <div class="muted" style="margin-top:4px">used {c.use_count} time(s)</div>
      </div>
      <Hint id="cred.delete">
        <button disabled={busy} onclick={() => act(() => api.deleteCredential(c.id))}>Forget</button>
      </Hint>
    </div>
  </div>
{/each}

<div class="card">
  <strong>Add a login</strong>
  <label for="l">What is it for?</label>
  <input id="l" bind:value={label} placeholder="Tennis club" />
  <label for="dm">Which site may it be used on?</label>
  <input id="dm" bind:value={domain} placeholder="club.example" />
  <label for="u">Username</label>
  <input id="u" bind:value={username} />
  <label for="s">Password</label>
  <input id="s" type="password" bind:value={secret} />
  <p class="muted" style="margin-top:8px">
    This goes straight to your keychain. Errand can use it; it cannot show it back to you, and it
    will never be typed into any site but the one above.
  </p>
  <div style="margin-top:10px">
    <Hint id="cred.add">
      <button class="primary" disabled={busy || !label.trim() || !domain.trim() || !secret} onclick={addCred}>
        Save to keychain
      </button>
    </Hint>
  </div>
</div>
