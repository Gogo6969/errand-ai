<script lang="ts">
  import { onMount } from "svelte";
  import { api, ApiError, type ChannelHealth, type Health, type Recipient } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";

  let health = $state<Health | null>(null);
  let channels = $state<ChannelHealth[]>([]);
  let notes = $state<Record<string, string>>({});
  let creds = $state<any[]>([]);
  let problem = $state<string | null>(null);
  let busy = $state(false);

  // Adding a login.
  let label = $state(""), domain = $state(""), username = $state(""), secret = $state("");

  // Telling Telegram who to talk to. Write-only: these go to the keychain and
  // Errand can never show them back.
  let tgToken = $state(""), tgChat = $state("");
  let waBase = $state("");

  // Quiet hours. Loaded from the daemon so the boxes show what is really set,
  // not a hopeful default.
  let quietFrom = $state(22), quietTo = $state(7), quietBreak = $state(true);

  // People this Mac may message on your behalf.
  let people = $state<Recipient[]>([]);
  let pLabel = $state(""), pChannel = $state("apple_mail"), pAddress = $state("");

  async function load() {
    try {
      health = await api.health();
      const c = await api.channels();
      channels = c.channels; notes = c.notes;
      creds = await api.credentials();
      try { people = await api.recipients(); } catch { people = []; }
      // Defensive: an older daemon has no settings endpoint, and the rest of
      // this page is still worth showing when it does not answer.
      try {
        const q = (await api.settings())["messaging.quiet"] as any;
        if (q) {
          quietFrom = q.from ?? quietFrom;
          quietTo = q.to ?? quietTo;
          quietBreak = q.failures_break_through ?? quietBreak;
        }
      } catch { /* leave the defaults showing */ }
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

  async function saveTelegram() {
    const secrets: Record<string, string> = {};
    if (tgToken.trim()) secrets["telegram.bot_token"] = tgToken.trim();
    if (tgChat.trim()) secrets["telegram.chat_id"] = tgChat.trim();
    await act(() => api.configureChannel("telegram", secrets));
    tgToken = ""; tgChat = "";
  }

  async function saveQuiet() {
    await act(() =>
      api.configureChannel("telegram", {}, {
        "messaging.quiet": {
          from: Number(quietFrom), to: Number(quietTo),
          failures_break_through: quietBreak,
        },
      }),
    );
  }

  async function saveWhatsapp() {
    await act(() =>
      api.configureChannel("whatsapp", {}, { "messaging.whatsapp.base_url": waBase.trim() }),
    );
    waBase = "";
  }

  async function addPerson() {
    await act(() => api.addRecipient(pLabel.trim(), pChannel, pAddress.trim()));
    pLabel = ""; pAddress = "";
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

    {#if c.channel === "telegram"}
      <div class="form">
        <label for="tgt">Bot token</label>
        <input id="tgt" type="password" bind:value={tgToken} autocomplete="off" placeholder="from @BotFather" />
        <label for="tgc">Chat id</label>
        <input id="tgc" bind:value={tgChat} placeholder="your own chat with the bot" />
        <p class="muted" style="margin:2px 0 0">
          Message @BotFather on Telegram to make a bot and get a token. Then message your new bot
          once and Errand can work out the chat id — or paste it here if you already know it.
          Both go straight to your keychain.
        </p>
        <Hint id="settings.save_telegram">
          <button disabled={busy || (!tgToken.trim() && !tgChat.trim())} onclick={saveTelegram}>
            Save Telegram details
          </button>
        </Hint>
      </div>
    {/if}

    {#if c.channel === "whatsapp"}
      <div class="form">
        <label for="wab">Gateway address</label>
        <input id="wab" bind:value={waBase} placeholder="http://127.0.0.1:3000" spellcheck="false" />
        <Hint id="settings.save_whatsapp">
          <button disabled={busy || !waBase.trim()} onclick={saveWhatsapp}>Save the address</button>
        </Hint>
      </div>
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
  <Hint id="settings.quiet"><strong>Quiet hours</strong></Hint>
  <p class="muted" style="margin:6px 0 10px">
    Messages to other people, and routine good news, wait until the quiet period ends.
  </p>
  <div class="quiet">
    <label for="qf">From</label>
    <input id="qf" type="number" min="0" max="23" bind:value={quietFrom} />
    <label for="qt">Until</label>
    <input id="qt" type="number" min="0" max="23" bind:value={quietTo} />
  </div>
  <label for="qb" class="check">
    <input id="qb" type="checkbox" bind:checked={quietBreak} />
    Still tell me straight away when something fails
  </label>
  <p class="muted" style="margin:6px 0 10px">
    Leave that on unless you have a reason not to: hearing at nine that the eight o'clock booking
    failed is hearing too late to do anything about it.
  </p>
  <Hint id="settings.save_quiet">
    <button disabled={busy} onclick={saveQuiet}>Save quiet hours</button>
  </Hint>
</div>

<h2>People Errand may message</h2>
<div class="card">
  <p class="muted" style="margin:0 0 10px">
    A task can send someone a message when it finishes. The agent can only ever reach people on
    this list and cannot type an address itself, so a web page cannot talk it into messaging
    anybody else. You choose per task who hears about it.
  </p>
  {#each people as p}
    <div class="row spread" style="margin-bottom:6px">
      <div>
        <strong>{p.label}</strong>
        <span class="muted"> · {p.channel.replace("_", " ")} · {p.address}</span>
      </div>
      <Hint id="settings.forget_person">
        <button disabled={busy} onclick={() => act(() => api.deleteRecipient(p.id))}>Forget</button>
      </Hint>
    </div>
  {/each}

  <div class="form">
    <label for="pl">Who is it?</label>
    <input id="pl" bind:value={pLabel} placeholder="Mum" />
    <label for="pc">How should Errand reach them?</label>
    <select id="pc" bind:value={pChannel}>
      <option value="apple_mail">Apple Mail</option>
      <option value="imessage">Apple Messages</option>
      <option value="whatsapp">WhatsApp</option>
      <option value="telegram">Telegram</option>
    </select>
    <label for="pa">Address</label>
    <input id="pa" bind:value={pAddress} placeholder={pChannel === "apple_mail" ? "name@example.com" : "+44 7700 900000"} />
    <Hint id="settings.add_person">
      <button disabled={busy || !pLabel.trim() || !pAddress.trim()} onclick={addPerson}>
        Save this person
      </button>
    </Hint>
  </div>
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

<style>
  /* The page had no styles of its own until now; the shared sheet in app.css
     covers everything else. These are only for the blocks added here. */
  .form { display: grid; gap: 4px; margin-top: 12px; max-width: 420px; }
  .quiet { display: flex; align-items: center; gap: 8px; margin-bottom: 4px; }
  .quiet label { margin: 0; }
  .quiet input { width: 72px; }
  .check { display: flex; align-items: center; gap: 8px; margin: 8px 0 0; cursor: pointer; }
  .check input { width: auto; }
</style>
