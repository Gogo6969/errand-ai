<script lang="ts">
  import { onMount } from "svelte";
  import Trouble from "$lib/components/Trouble.svelte";
  import { reconnecting } from "$lib/reconnect.svelte";
  import {
    api, ApiError, channelName,
    type AutomationApp, type ChannelHealth, type Health, type Recipient,
  } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";

  let health = $state<Health | null>(null);
  let channels = $state<ChannelHealth[]>([]);
  let notes = $state<Record<string, string>>({});

  // Apps on this Mac a task drives, which are not channels: nothing here
  // messages anybody. They need the same macOS permission and so they need the
  // same button, in the same place, or nobody ever finds it.
  let apps = $state<AutomationApp[]>([]);
  let appNotes = $state<Record<string, string>>({});
  let creds = $state<any[]>([]);
  let problem = $state<string | null>(null);
  let busy = $state(false);

  // Adding a login.
  let label = $state(""), domain = $state(""), username = $state(""), secret = $state("");
  // Off by default. A password field that starts visible is one somebody types
  // into with a colleague behind them; the toggle is there for the other case,
  // where a login is refused twice and nobody can see why.
  let showSecret = $state(false);

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

  // Where Errand reaches you, one box per channel. Typed but not yet saved, so
  // a half-entered number is never the address a test is sent to.
  let selfDraft = $state<Record<string, string>>({});

  // What happened last time a button on a channel's card was pressed. Kept per
  // card rather than in the box at the top of the page: somebody who pressed
  // Send test and watched the button saw nothing happen at all, and concluded
  // the button was broken.
  let outcome = $state<Record<string, { ok: boolean; text: string }>>({});

  // Which button is mid-flight, as "{channel}:{what}", so pressing one visibly
  // does something before the answer comes back.
  let working = $state("");

  async function load() {
    {
      health = await api.health();
      const c = await api.channels();
      channels = c.channels; notes = c.notes;
      // Only fill in the boxes nobody has touched: loading happens after every
      // action on this page, and wiping a half-typed number would be its own
      // small betrayal.
      for (const ch of channels) selfDraft[ch.channel] ??= "";
      // Defensive: an older daemon knows about channels and not about the apps
      // a task drives, and the rest of this page is still worth showing.
      try {
        const a = await api.automation();
        apps = a.apps; appNotes = a.notes;
      } catch { apps = []; }
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
    }
  }

  const conn = reconnecting(load, (p) => (problem = p));
  onMount(() => {
    conn.run();
    return conn.stop;
  });

  async function act(fn: () => Promise<unknown>) {
    busy = true;
    try { await fn(); await load(); }
    catch (e) { problem = e instanceof ApiError ? e.message : String(e); }
    finally { busy = false; }
  }

  /**
   * Do something to one card and answer on that card.
   *
   * Used by the channels and by the apps below them, because a permission
   * refused reads the same either way. Deliberately not act(): a failure from
   * this button belongs beside the button, not in the box at the top of the
   * page where nobody watching the button will see it.
   */
  async function cardAct(
    card: string,
    what: string,
    fn: () => Promise<unknown>,
    said: (result: any) => string | { ok: boolean; text: string },
  ): Promise<boolean> {
    working = `${card}:${what}`;
    busy = true;
    try {
      const result = await fn();
      // A call that came back is not the same as a thing that worked, so the
      // caller may say otherwise: macOS answers a permission request without
      // necessarily having granted it.
      const answer = said(result);
      outcome[card] = typeof answer === "string" ? { ok: true, text: answer } : answer;
      await load();
      return true;
    } catch (e) {
      outcome[card] = { ok: false, text: e instanceof ApiError ? e.message : String(e) };
      return false;
    } finally {
      working = ""; busy = false;
    }
  }

  function enableChannel(c: ChannelHealth) {
    return cardAct(
      c.channel, "enable",
      () => api.enableChannel(c.channel),
      // The daemon's own words for what it found, rather than a hopeful
      // "Enabled" printed in green over a permission macOS quietly refused.
      (h) => ({
        ok: h?.status === "ok",
        text: typeof h?.detail === "string" ? h.detail : `${channelName(c)} is switched on.`,
      }),
    );
  }

  function enableApp(a: AutomationApp) {
    return cardAct(
      a.app, "enable",
      () => api.enableAutomation(a.app),
      // The daemon's own words for what it found. macOS can answer a request
      // without having granted anything, and "Enabled" printed in green over
      // that is a lie the person only discovers at three in the morning.
      (h) => ({
        ok: h?.status === "ok",
        text: typeof h?.detail === "string" ? h.detail : `${a.display_name} is switched on.`,
      }),
    );
  }

  function testChannel(c: ChannelHealth) {
    const to = c.self_address ?? (c.channel === "telegram" ? "your Telegram chat" : "you");
    return cardAct(
      c.channel, "test",
      () => api.testChannel(c.channel),
      () =>
        `Sent to ${to}. If it does not arrive, it left Errand and something between here and ` +
        `there dropped it.`,
    );
  }

  async function saveSelf(c: ChannelHealth) {
    const value = (selfDraft[c.channel] ?? "").trim();
    if (!value) return;
    const saved = await cardAct(
      c.channel, "self",
      () => api.configureChannel(c.channel, {}, { [`messaging.self.${c.channel}`]: value }),
      () => `Saved. Tests on ${channelName(c)} now go to ${value}.`,
    );
    // Keep what was typed when it did not save, so it can be corrected rather
    // than typed again.
    if (saved) selfDraft[c.channel] = "";
  }

  /** What "you" is called on each channel. The wrong word here reads as a recipient. */
  function selfThing(channel: string): string {
    return channel === "apple_mail" ? "email address"
      : channel === "telegram" ? "Telegram chat id"
      : "phone number";
  }

  function selfExample(channel: string): string {
    return channel === "apple_mail" ? "you@example.com"
      : channel === "telegram" ? "123456789"
      : "+44 7700 900000";
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
    showSecret = false;
  }

  /**
   * Changing a saved login.
   *
   * The password box starts empty and stays empty unless something is typed in
   * it, because there is nothing to prefill it with: the stored password cannot
   * be read back, by this window or by anything else. Left empty it is left
   * alone, so somebody correcting a typo in a username does not have to know
   * the password to do it.
   */
  let editing = $state<string | null>(null);
  let eLabel = $state("");
  let eUsername = $state("");
  let eSecret = $state("");
  let showESecret = $state(false);

  function startEdit(c: { id: string; label: string; username?: string | null }) {
    editing = c.id;
    eLabel = c.label;
    eUsername = c.username ?? "";
    eSecret = "";
    showESecret = false;
  }

  function stopEdit() {
    editing = null;
    eSecret = "";
    showESecret = false;
  }

  async function saveCred(id: string) {
    await act(() =>
      api.updateCredential(id, {
        label: eLabel.trim(),
        username: eUsername.trim(),
        ...(eSecret ? { secret: eSecret } : {}),
      }),
    );
    stopEdit();
  }

  const cls = (s: string) => (s === "ok" ? "ok" : s === "not_configured" ? "" : s === "needs_user" ? "warn" : "bad");

  // A recipient carries only the channel's id, and "imessage" is not a word
  // anybody uses. The names come from the daemon rather than a second list here
  // that would drift away from it.
  const named = $derived(Object.fromEntries(channels.map((c) => [c.channel, channelName(c)])));
</script>

<h1>Settings</h1>
{#if problem}
  <Trouble {problem} retrying={conn.retrying} onRetry={() => conn.run(true)} />
{/if}

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
<p class="deck">
  Ways Errand talks to <strong>you</strong>. Everybody else lives under "People Errand may message"
  further down this page.
</p>
{#each channels as c}
  <div class="card">
    <div class="row spread">
      <div>
        <Hint id={c.channel === "telegram" ? "channel.telegram" : c.channel === "whatsapp" ? "channel.whatsapp" : "channel.apple"}>
          <strong>{channelName(c)}</strong>
        </Hint>
        <div class="muted" style="margin-top:4px">{c.detail}</div>
        {#if c.fix}<div class="muted" style="margin-top:4px; color:var(--warn)">{c.fix}</div>{/if}
      </div>
      <div class="row">
        {#if c.channel === "apple_mail" || c.channel === "imessage"}
          <Hint id="channel.apple">
            <button disabled={busy} onclick={() => enableChannel(c)}>
              {working === `${c.channel}:enable` ? "Asking macOS…" : "Enable"}
            </button>
          </Hint>
        {/if}
        <Hint id="channel.test">
          <button disabled={busy} onclick={() => testChannel(c)}>
            {working === `${c.channel}:test` ? "Sending…" : c.self_address ? `Send test to ${c.self_address}` : "Send test"}
          </button>
        </Hint>
        <span class="pill {cls(c.status)}">{c.status.replace("_", " ")}</span>
      </div>
    </div>

    <!-- What those buttons did, beside the buttons that did it. -->
    {#if outcome[c.channel]}
      {@const o = outcome[c.channel]}
      <div class="result {o.ok ? 'good' : 'bad'}">{o.text}</div>
    {/if}

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
          once and Errand can work out the chat id, or paste it here if you already know it.
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

    <!-- Somebody tried to fix a failing test by adding a recipient, which is a
         different thing entirely, so the difference is said here in the page
         rather than left to a tooltip nobody hovers. -->
    <div class="form yours">
      <label for="self-{c.channel}">Your own {selfThing(c.channel)}</label>
      <input id="self-{c.channel}" bind:value={selfDraft[c.channel]} placeholder={selfExample(c.channel)} spellcheck="false" autocomplete="off" />
      <p class="muted" style="margin:2px 0 0">
        Where Errand reaches <strong>you</strong>, and the only place a test is ever sent. Other
        people go under "People Errand may message" below.
      </p>
      {#if c.self_address}
        <p class="muted" style="margin:2px 0 0">Tests currently go to {c.self_address}.</p>
      {:else if c.channel === "telegram"}
        <p class="muted" style="margin:2px 0 0">
          Not set, so tests go to the chat id saved above. Fill this in only if they should go
          somewhere else.
        </p>
      {:else}
        <p class="muted" style="margin:2px 0 0; color:var(--warn)">
          Not set, so Send test has nowhere to go and will fail until you fill this in.
        </p>
      {/if}
      <Hint id="settings.save_self">
        <button disabled={busy || !(selfDraft[c.channel] ?? "").trim()} onclick={() => saveSelf(c)}>
          {working === `${c.channel}:self` ? "Saving…" : `Save my ${selfThing(c.channel)}`}
        </button>
      </Hint>
    </div>
  </div>
{/each}

<h2>Apps on this Mac</h2>
<p class="deck">
  What a task may do on the Mac itself: write you a note, or go through your post. macOS grants
  this to whichever program asks, so Errand asks from here, while you are looking at the screen.
  Asking at three in the morning puts the question where nobody can answer it, and the task simply
  stops.
</p>
{#each apps as a}
  <div class="card">
    <div class="row spread">
      <div>
        <Hint id="automation.what"><strong>{a.display_name}</strong></Hint>
        <div class="muted" style="margin-top:4px">{a.detail}</div>
        {#if a.fix}<div class="muted" style="margin-top:4px; color:var(--warn)">{a.fix}</div>{/if}
        {#if appNotes[a.app]}
          <div class="muted" style="margin-top:6px">{appNotes[a.app]}</div>
        {/if}
      </div>
      <div class="row">
        <Hint id="automation.enable">
          <button disabled={busy} onclick={() => enableApp(a)}>
            {working === `${a.app}:enable` ? "Asking macOS…" : "Enable"}
          </button>
        </Hint>
        <span class="pill {cls(a.status)}">{a.status.replace("_", " ")}</span>
      </div>
    </div>

    <!-- What that button did, beside the button that did it. -->
    {#if outcome[a.app]}
      {@const o = outcome[a.app]}
      <div class="result {o.ok ? 'good' : 'bad'}">{o.text}</div>
    {/if}
  </div>
{/each}

<h2>Letting other programs in</h2>
<div class="card">
  <Hint id="settings.token"><span>Access key</span></Hint>
  <p class="muted" style="margin:6px 0 0">
    Another program needs a key to talk to Errand. Create one from a terminal with
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
    Other people. A task can send them a message when it finishes. The agent can only ever reach
    people on this list and cannot type an address itself, so a web page cannot talk it into
    messaging anybody else. You choose per task who hears about it.
  </p>
  <p class="muted" style="margin:0 0 10px">
    Adding someone here does not tell Errand where to reach <strong>you</strong>, and does not make
    Send test work. That is "Your own …" on each channel's card at the top of this page.
  </p>
  {#each people as p}
    <div class="row spread" style="margin-bottom:6px">
      <div>
        <strong>{p.label}</strong>
        <span class="muted"> · {named[p.channel] ?? p.channel.replace("_", " ")} · {p.address}</span>
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
<!-- One eye, drawn twice: on the field a new password is typed into and on the
     field one is replaced in. It shows what is being typed now, and never what
     was typed before, which is the whole distinction this screen rests on. -->
{#snippet eye(shown: boolean)}
  <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"
    fill="none" stroke="currentColor" stroke-width="1.7"
    stroke-linecap="round" stroke-linejoin="round">
    <path d="M2 12s3.6-6.5 10-6.5S22 12 22 12s-3.6 6.5-10 6.5S2 12 2 12z" />
    <circle cx="12" cy="12" r="2.8" />
    {#if shown}<path d="M4 20 20 4" />{/if}
  </svg>
{/snippet}

{#each creds as c}
  <div class="card">
    {#if editing === c.id}
      <strong>Change this login</strong>
      <label for={`el-${c.id}`}>What is it for?</label>
      <input id={`el-${c.id}`} bind:value={eLabel} />
      <label for={`eu-${c.id}`}>Username</label>
      <input id={`eu-${c.id}`} bind:value={eUsername} />
      <label for={`es-${c.id}`}>New password</label>
      <div class="secretrow">
        <input
          id={`es-${c.id}`}
          type={showESecret ? "text" : "password"}
          bind:value={eSecret}
          placeholder="Leave empty to keep the saved one"
        />
        <Hint id="cred.reveal">
          <button
            type="button"
            class="reveal"
            aria-pressed={showESecret}
            aria-label={showESecret ? "Hide the password" : "Show the password"}
            onclick={() => (showESecret = !showESecret)}
          >
            {@render eye(showESecret)}
          </button>
        </Hint>
      </div>
      <p class="muted" style="margin-top:8px">
        The saved password cannot be shown here, or anywhere else: it is in your keychain and
        Errand can only type it into {c.domain}. Typing a new one replaces it. If you have
        forgotten it, change it at the site and then put the new one here.
      </p>
      <div class="row" style="margin-top:10px; gap:6px">
        <Hint id="cred.save_change">
          <button class="primary" disabled={busy || !eLabel.trim()} onclick={() => saveCred(c.id)}>
            Save
          </button>
        </Hint>
        <Hint id="cred.cancel_change">
          <button disabled={busy} onclick={stopEdit}>Never mind</button>
        </Hint>
      </div>
    {:else}
      <div class="row spread">
        <div>
          <strong>{c.label}</strong>
          <Hint id="cred.domain"><span class="muted"> · only on {c.domain}</span></Hint>
          <div class="muted" style="margin-top:4px">
            {c.username ? `${c.username} · ` : ""}used {c.use_count} time(s)
          </div>
        </div>
        <div class="row" style="gap:6px">
          <Hint id="cred.change">
            <button disabled={busy} onclick={() => startEdit(c)}>Change</button>
          </Hint>
          <Hint id="cred.delete">
            <button disabled={busy} onclick={() => act(() => api.deleteCredential(c.id))}>Forget</button>
          </Hint>
        </div>
      </div>
    {/if}
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
  <div class="secretrow">
    <input id="s" type={showSecret ? "text" : "password"} bind:value={secret} />
    <Hint id="cred.reveal">
      <button
        type="button"
        class="reveal"
        aria-pressed={showSecret}
        aria-label={showSecret ? "Hide the password" : "Show the password"}
        onclick={() => (showSecret = !showSecret)}
      >
        {@render eye(showSecret)}
      </button>
    </Hint>
  </div>
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

  /* The password field and its eye, on one line, with the eye outside the box
     rather than floating inside it: inside, it sits on top of the text it is
     there to reveal. */
  .secretrow { display: flex; align-items: center; gap: 6px; }
  .secretrow input { flex: 1; min-width: 0; }
  .reveal {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    padding: 0;
    flex: none;
    color: var(--ink-soft);
  }
  .reveal:hover, .reveal[aria-pressed="true"] { color: var(--ink); }
  /* The answer to the button directly above it, tinted so it cannot be mistaken
     for the description of the channel. */
  .result {
    margin-top: 10px; padding: 8px 11px; border-radius: 6px; font-size: 12.5px;
    background: var(--surface-2); color: var(--ink-soft); border-left: 3px solid var(--rule);
  }
  .result.good { background: var(--ok-bg); color: var(--ok); border-left-color: var(--ok); }
  .result.bad { background: var(--bad-bg); color: var(--bad); border-left-color: var(--bad); }
  /* A rule down the side, because this box and the recipient list below it are
     two different things and were being confused for one. */
  .yours { border-left: 3px solid var(--accent); padding-left: 11px; }
  .quiet { display: flex; align-items: center; gap: 8px; margin-bottom: 4px; }
  .quiet label { margin: 0; }
  .quiet input { width: 72px; }
  .check { display: flex; align-items: center; gap: 8px; margin: 8px 0 0; cursor: pointer; }
  .check input { width: auto; }
</style>
