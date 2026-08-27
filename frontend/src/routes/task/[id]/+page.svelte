<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import Trouble from "$lib/components/Trouble.svelte";
  import { reconnecting } from "$lib/reconnect.svelte";
  import { page } from "$app/state";
  import { api, channelName, followRun, statusLabel, when, ApiError, type AiSetup, type ListedProvider, type Task, type Run, type TaskRecipient, type Recipient, type MailGrant } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";
  import { headlineFor, hintFor, ranHeadline } from "$lib/taskState";
  import ScheduleEditor from "$lib/components/ScheduleEditor.svelte";
  import SitesEditor from "$lib/components/SitesEditor.svelte";

  const id = page.params.id!;
  let task = $state<Task | null>(null);
  let runs = $state<Run[]>([]);
  let plan = $state<any>(null);
  let problem = $state<string | null>(null);
  // Closed by default, which is the whole point: a task is opened to see what
  // it produced, not to be asked eight questions about how it should work.
  let showSettings = $state(false);
  let settingsTop: HTMLElement | undefined = $state();

  /**
   * Open the settings, and go to them.
   *
   * The going is the part that was missing. Settings sit below the answer, and
   * an answer can be a screen and a half of trending topics, so the panel
   * opened underneath everything the reader could see and the gear looked
   * broken. It was doing exactly what it was told; there was simply nothing to
   * notice. Reported as "clicking the gear does nothing", which is what it is
   * from the chair.
   */
  function toggleSettings() {
    showSettings = !showSettings;
    if (!showSettings) return;
    // One frame, so the panel exists to be scrolled to.
    requestAnimationFrame(() =>
      settingsTop?.scrollIntoView({
        behavior: window.matchMedia?.("(prefers-reduced-motion: reduce)").matches
          ? "auto"
          : "smooth",
        block: "start",
      })
    );
  }
  let draftAnswer = $state("");
  let editingDirective = $state(false);
  let draftDirective = $state("");
  let busy = $state(false);

  // Editing state. Nothing is saved until Save is pressed, so a half-typed site
  // never becomes a rule the agent is bound by.
  let editingSchedule = $state(false);
  let draftSchedule = $state<any>(null);
  let editingSites = $state(false);
  let draftSites = $state<string[]>([]);
  let editingLimits = $state(false);
  let draftLimits = $state({
    max_steps: 60,
    max_minutes: 15,
    max_usd: 0.5,
    max_messages: 3,
    max_spend_usd: 0,
  });
  let warnings = $state<string[]>([]);
  // A refusal the person can answer. The daemon refuses a schedule change that
  // might repeat something irreversible and says to send it again with an
  // acknowledgement, so there has to be something to press.
  let mustConfirm = $state<{ detail: string; patch: any } | null>(null);
  let people = $state<TaskRecipient[]>([]);
  let everyone = $state<Recipient[]>([]);
  // Null means the daemon would not say, so the section is left off the page
  // rather than drawn as "not allowed", which would be a claim rather than a
  // silence.
  let mail = $state<MailGrant | null>(null);
  let channelNames = $state<Record<string, string>>({});
  const named = (channel: string) => channelNames[channel] ?? channel.replace(/_/g, " ");

  // What the AI screen is set up with, so this page can name the default rather
  // than saying "Default" and leaving somebody to go and look. Null means the
  // daemon would not say, and then the choice is left off the page: offering a
  // list of models nobody could fill in would be worse than not offering one.
  let ai = $state<AiSetup | null>(null);
  const executorRole = $derived(ai?.roles.find((r) => r.role === "executor") ?? null);
  const defaultModel = $derived(executorRole?.using?.display_name ?? null);
  // The model this task names, when it names one that is still in the list.
  const taskModel = $derived(
    ai?.providers.find((p) => p.id === task?.model_id) ?? null,
  );
  /**
   * Would this task's choice keep the work on this machine?
   *
   * Only said where it matters, which is a task that has been handed something
   * private. The model doing the job is the thing that reads what the job
   * reads, and that is the whole reason for choosing one per task.
   */
  const readsSomethingPrivate = $derived(mail?.granted ?? false);

  /**
   * What to say after a model's name in the list of who could do this task.
   *
   * The same three answers the AI screen gives, because they are answers to the
   * same question and two screens disagreeing about a model is worse than
   * either of them being wrong.
   */
  function modelNote(p: ListedProvider): string {
    if (p.tools === "no") return " (cannot use tools)";
    if (p.cannot_carry_out_because) return " (cannot do this job)";
    if (!p.enabled) return " (switched off)";
    if (p.tools === "unknown") return " (not checked yet)";
    return "";
  }

  async function chooseModel(modelId: string) {
    // Null rather than the empty string the menu's first option carries, so
    // what reaches the daemon says "back to the default" in as many words.
    // A refused choice is reloaded rather than left showing in the menu: the
    // task is not using it, and a menu that says otherwise is a lie.
    if (!(await save({ model_id: modelId || null }))) await load();
  }

  // A run that has stopped. Anything else is still going, which is the thing
  // somebody standing at this page actually wants to know.
  const FINISHED = ["succeeded", "failed", "cancelled", "skipped"];

  // How many steps the run in flight has taken. Fetched once, then moved by the
  // live stream, so a number on screen going up is itself the answer to "is it
  // doing anything".
  let liveSteps = $state(0);
  // Which run is being followed. Deliberately not $state: the effect below reads
  // it, and a reactive read there would keep waking the effect up.
  let following: string | null = null;
  let stopFollow: (() => void) | undefined;
  let heartbeat: any;

  const newest = $derived(runs[0] ?? null);
  const liveRun = $derived(newest && !FINISHED.includes(newest.status) ? newest : null);
  const finishedRun = $derived(newest && FINISHED.includes(newest.status) ? newest : null);

  /**
   * The status to put at the top of the page.
   *
   * The word stored on the task lags the run that is meant to change it, and a
   * task still saying "Learning" hours after its teaching run failed is the
   * exact lie this page was rebuilt to stop telling. Where the newest run
   * disagrees with the task, the run is the one that knows.
   */
  const headline = $derived(headlineFor(task, liveRun ?? finishedRun));

  /**
   * Do we already know this task can do something it cannot take back?
   *
   * Only two of those are visible from here: moving somebody's post, and
   * writing to a real person. Whether the job books a court is in the
   * description and nothing on this page can read it, so the advice below is
   * offered for every untaught task and only sharpened when we do know.
   */
  const knownIrreversible = $derived((mail?.may_file ?? false) || people.length > 0);

  /// Has this task ever really done the job?
  ///
  /// The one thing standing between a task and running while nobody watches.
  /// A rehearsal does not count: it is told to carry on as though everything
  /// worked and touches nothing, so it lands as a success having proved
  /// nothing. The daemon asks the same question of the database; this only
  /// decides which button to show.
  const hasEverRun = $derived(
    runs.some((r) => r.status === "succeeded" && !r.rehearsal && r.mode !== "dry_run"),
  );

  /// What sort of schedule it has. The task's schedule is untyped JSON on the
  /// wire, and only its kind matters here.
  const scheduleKind = $derived(
    (task?.schedule as { kind?: string } | undefined)?.kind ?? "manual",
  );

  /// Offer to put it on its schedule once it has been seen working, and only
  /// while it is not already on one.
  const canBeScheduled = $derived(
    hasEverRun && task?.status !== "ready" && scheduleKind !== "manual",
  );

  /// "every day at 07:00", out of the engine's own words for the schedule, so
  /// the button says what it will actually do.
  const scheduleClause = $derived(
    (task?.schedule_describes ?? "")
      .split(".")[0]
      .replace(/^Every /, "every ")
      .replace(/^On the schedule.*/, "on its schedule")
      .trim(),
  );


  async function countSteps(runId: string) {
    try {
      const detail = await api.runDetail(runId);
      liveSteps = detail.steps.length;
      // The stream can be missed entirely, so this has to be able to notice an
      // ending on its own rather than trusting an event that never came.
      if (FINISHED.includes(detail.status)) await load();
    } catch {
      // The step count is a comfort, not the point. A page that still works
      // without it beats a page that breaks over it.
    }
  }

  $effect(() => {
    const runId = liveRun?.id ?? null;
    if (runId === following) return;
    stopFollow?.();
    stopFollow = undefined;
    following = runId;
    liveSteps = 0;
    if (!runId) return;
    countSteps(runId);
    stopFollow = followRun(runId, (e) => {
      const data = e.data as any;
      if (typeof data?.seq === "number") liveSteps = Math.max(liveSteps, data.seq);
      // The moment a run ends, everything else on this page is out of date.
      if (e.event === "run.finished" || e.event === "run.failed") load();
      if (e.event === "run.status" && FINISHED.includes(data?.status)) load();
    });
  });

  /// Everything this page shows, in one place.
  ///
  /// Throws when it cannot be read, rather than swallowing it: that is what
  /// lets the wrapper tell a service that is not there from an answer that is
  /// genuinely empty, and try again in a moment.
  async function loadEverything() {
    {
      [task, runs, plan] = await Promise.all([api.task(id), api.runs(id), api.playbook(id)]);
      // Recipients are optional plumbing: a task page must still render when
      // nobody has set up a way to message anyone.
      try {
        [people, everyone] = await Promise.all([api.taskRecipients(id), api.recipients()]);
      } catch { people = []; everyone = []; }
      try {
        mail = await api.mailGrant(id);
      } catch { mail = null; }
      // Optional in the same way: a task page must still open when the daemon
      // cannot say what models it has.
      try {
        ai = await api.ai();
      } catch { ai = null; }
      // A recipient carries only a channel's id, and "imessage" is not a word
      // anybody uses. The names come from the daemon rather than a second list
      // kept here, which would drift away from it.
      try {
        const known = await api.channels();
        channelNames = Object.fromEntries(known.channels.map((c) => [c.channel, channelName(c)]));
      } catch { /* the id reads badly, but it beats a blank where a name should be */ }
    }
  }

  const conn = reconnecting(loadEverything, (p) => (problem = p));
  const load = conn.run;
  onMount(() => {
    conn.run();
    // A slow safety net, not the mechanism. If the stream drops, a run that
    // ended would otherwise sit at the top of this page still claiming to be
    // working, which is the failure this whole section exists to prevent.
    heartbeat = setInterval(() => { if (following) countSteps(following); }, 20000);
  });

  onDestroy(() => {
    stopFollow?.();
    clearInterval(heartbeat);
  });

  async function save(patch: Parameters<typeof api.patchTask>[1]) {
    busy = true;
    try {
      const res = await api.patchTask(id, patch);
      warnings = res.warnings ?? [];
      problem = null;
      mustConfirm = null;
      await load();
      return true;
    } catch (e) {
      if (e instanceof ApiError && e.code === "schedule_change_may_repeat") {
        // Not an error to read and give up on: a question with two answers.
        mustConfirm = { detail: e.message, patch };
        problem = null;
      } else {
        problem = e instanceof ApiError ? e.message : String(e);
      }
      return false;
    } finally {
      busy = false;
    }
  }

  async function confirmRepeat() {
    const pending = mustConfirm;
    if (!pending) return;
    mustConfirm = null;
    if (await save({ ...pending.patch, acknowledge_repeat: true })) editingSchedule = false;
  }

  /// The last run, if it stopped to ask rather than to fail.
  const askedRun = $derived(
    finishedRun?.failure?.code === "needs_answer" ? finishedRun : null,
  );

  async function sendAnswer() {
    const text = draftAnswer.trim();
    if (!text || !askedRun) return;
    await act(async () => {
      await api.answerQuestion(askedRun.id, text);
      draftAnswer = "";
      await api.run(id);
    });
  }

  /// Put this task away, or forget it entirely.
  ///
  /// Asked as one question with two answers rather than a checkbox, because
  /// the difference matters and is easy to skip past: putting it away keeps
  /// the record of anything it booked or bought, and that record is what stops
  /// a later run doing it twice.
  async function removeThis() {
    if (!task) return;
    const everRan = runs.length > 0;
    const away = confirm(
      `Stop "${task.name}" for good?\n\n` +
        (everRan
          ? "What it did is kept, including the record that stops a later run repeating anything it booked or bought."
          : "It never ran, so there is nothing to keep."),
    );
    if (!away) return;
    // Only offered for a task with nothing worth keeping, so the destructive
    // answer is never the easy one to give by accident.
    const forget =
      !everRan &&
      confirm(`Remove "${task.name}" completely, rather than putting it away?`);
    await act(async () => {
      await api.removeTask(id, forget);
      location.href = "/";
    });
  }

  function startEditingDirective() {
    draftDirective = task?.description ?? "";
    editingDirective = true;
  }

  /// Save the new wording and do the job again with it.
  ///
  /// Running it straight away is the point. Somebody changing the directive is
  /// answering "that was not what I meant", and the only thing that settles
  /// whether the new wording is better is another outcome.
  async function saveDirective() {
    const text = draftDirective.trim();
    if (!text || text === task?.description) {
      editingDirective = false;
      return;
    }
    if (!(await save({ description: text }))) return;
    editingDirective = false;
    await act(() => api.run(id));
  }

  async function saveSchedule() {
    if (await save({ schedule: draftSchedule })) editingSchedule = false;
  }
  async function saveSites() {
    if (await save({ allowed_domains: draftSites })) editingSites = false;
  }
  async function saveLimits() {
    const limits = {
      max_steps: Math.max(0, Math.round(draftLimits.max_steps)),
      max_minutes: Math.max(0, Math.round(draftLimits.max_minutes)),
      max_usd: Math.max(0, draftLimits.max_usd),
      max_messages: Math.max(0, Math.round(draftLimits.max_messages)),
      max_spend_usd: Math.max(0, draftLimits.max_spend_usd),
    };
    if (await save({ limits })) editingLimits = false;
  }

  function startEditLimits() {
    // Prefill from what the daemon reports, falling back to its defaults, so
    // the form never invents a number the daemon would not recognise.
    draftLimits = {
      max_steps: task?.limits?.max_steps ?? 60,
      max_minutes: task?.limits?.max_minutes ?? 15,
      max_usd: task?.limits?.max_usd ?? 0.5,
      max_messages: task?.limits?.max_messages ?? 3,
      max_spend_usd: task?.limits?.max_spend_usd ?? 0,
    };
    editingLimits = true;
  }

  // The link is an upsert on the daemon's side, so changing when somebody
  // hears is the same call as granting it in the first place.
  async function togglePerson(p: TaskRecipient, which: "success" | "failure") {
    const onSuccess = which === "success" ? !p.on_success : p.on_success;
    const onFailure = which === "failure" ? !p.on_failure : p.on_failure;
    if (!onSuccess && !onFailure) return; // "hears about nothing" is Remove, not a toggle
    await act(() => api.linkRecipient(id, p.id, onSuccess, onFailure));
  }

  const unlinked = $derived(everyone.filter((r) => !people.some((p) => p.id === r.id)));

  // Granting again with a different answer is how the moving half is turned on
  // and off, the same upsert the recipient links use.
  async function setMailAccess(mayFile: boolean) {
    await act(() => api.grantMail(id, mayFile));
  }

  // The report a finished run sends shares the task's message budget with the
  // agent's own sends, so linking more people than the limit allows means some
  // of them are simply not told. Better to say so here than to let it be
  // discovered from a line in a run timeline.
  const tooManyPeople = $derived(
    task?.limits?.max_messages !== undefined && people.length > task.limits.max_messages,
  );

  async function act(fn: () => Promise<unknown>) {
    busy = true;
    try { await fn(); await load(); }
    catch (e) { problem = e instanceof ApiError ? e.message : String(e); }
    finally { busy = false; }
  }
</script>

{#if problem}
  <Trouble {problem} retrying={conn.retrying} onRetry={() => conn.run(true)} />
{/if}

{#if mustConfirm}
  <div class="err">
    <h3>This could happen twice</h3>
    <div>{mustConfirm.detail}</div>
    <div class="row" style="margin-top:10px; gap:6px">
      <Hint id="task.confirm_repeat">
        <button disabled={busy} onclick={confirmRepeat}>Change it anyway</button>
      </Hint>
      <Hint id="task.cancel_edit">
        <button disabled={busy} onclick={() => (mustConfirm = null)}>Leave it alone</button>
      </Hint>
    </div>
  </div>
{/if}

<!-- Warnings outlive the editor they came from: a site list that will not work
     is worth seeing every time the task is opened, not only in the two seconds
     after saving it. -->
{#if warnings.length}
  <div class="warnbox">
    {#each warnings as w}<div>{w}</div>{/each}
  </div>
{/if}

{#if task}
  <div class="row spread">
    <h1>{task.emoji ?? ""} {task.name}</h1>
    <div class="row" style="gap:10px">
      <Hint id={hintFor(task, liveRun ?? finishedRun)}><span class="pill {headline.cls}">{headline.text}</span></Hint>
      <Hint id="task.settings">
        <button
          class="gear"
          aria-expanded={showSettings}
          aria-label={showSettings ? "Hide task settings" : "Task settings"}
          onclick={toggleSettings}
        >
          <svg viewBox="0 0 24 24" width="17" height="17" aria-hidden="true"
            fill="none" stroke="currentColor" stroke-width="1.7"
            stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="3.2" />
            <path d="M19.9 14.7a1.7 1.7 0 0 0 .3 1.9l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.5v.2a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.6 1.7 1.7 0 0 0-1.9.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.9 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.6-1.1 1.7 1.7 0 0 0-.3-1.9l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.9.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.9-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.9V9a1.7 1.7 0 0 0 1.5 1h.2a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z" />
          </svg>
        </button>
      </Hint>
    </div>
  </div>
  <!-- The directive itself, editable in place.
       Reading the outcome and wanting to change what you asked for is one
       thought, not two, and it should not need a settings screen: what the
       task says is the task, and everything else on this page is machinery. -->
  {#if editingDirective}
    <textarea class="directive" rows="4" bind:value={draftDirective} data-hint-exempt="the task's own words being edited; nothing happens until you save"></textarea>
    <div class="row" style="margin-top:8px">
      <Hint id="task.directive_save">
        <button class="primary" disabled={busy || !draftDirective.trim()} onclick={saveDirective}>
          Save and run it again
        </button>
      </Hint>
      <button disabled={busy} onclick={() => (editingDirective = false)} data-hint-exempt="closes the editor above, changes nothing">Cancel</button>
    </div>
  {:else}
    <p class="deck">
      {task.description}
      <Hint id="task.directive">
        <button class="linkish" onclick={startEditingDirective}>Change what you asked for</button>
      </Hint>
    </p>
  {/if}

  <!-- Before anything else this page has to answer two questions: is it doing
       anything, and if it has stopped, what happened. Neither should need
       anybody to know what a run is, or to go looking in History for it. -->
  {#if liveRun}
    <div class="card now">
      <div class="row spread">
        <div>
          <strong><span class="blip"></span>Working on it now</strong>
          <div class="muted" style="margin-top:4px">
            {statusLabel(liveRun.status)} ·
            {liveSteps === 0 ? "getting started" : `${liveSteps} step${liveSteps === 1 ? "" : "s"} so far`}
            · began {when(liveRun.started_at ?? liveRun.created_at)}
          </div>
        </div>
        <Hint id="task.watch_run">
          <a class="golink" href={`/run/${liveRun.id}`}>Watch it</a>
        </Hint>
      </div>
    </div>
  {:else if finishedRun}
    <!-- The answer first, and whole.
         This card used to show only what the run DID, in one clipped line,
         while the thing the task was set up to produce went into a note. A
         person opening a finished task should read the answer before anything
         else, and should not have to open a second screen to find it.

         A failed run gets the same treatment when it has one: the common
         failure is a run that read everything, worked out the answer, and only
         then found the Mac would not let it write the note. Hiding what it
         found behind the failure makes somebody do that work again. -->
    <div class="card ended" class:bad={finishedRun.status === "failed"}>
      {#if finishedRun.answer}
        <div class="answer">{finishedRun.answer}</div>
      {/if}
      <div class="row spread" style="margin-top:{finishedRun.answer ? '12px' : '0'}">
        <div>
          <strong>{ranHeadline(finishedRun)}</strong>
          <span class="muted"> · {when(finishedRun.finished_at ?? finishedRun.created_at)}</span>
          {#if finishedRun.failure}
            <div style="margin-top:6px">{finishedRun.failure.plain_reason}</div>
            {#if finishedRun.failure.fix}
              <div style="margin-top:4px; font-weight:500">{finishedRun.failure.fix}</div>
            {/if}
          {:else if finishedRun.summary}
            <div class="muted" style="margin-top:6px">{finishedRun.summary.split("\n")[0]}</div>
          {/if}
        </div>
        <Hint id="task.last_run">
          <a class="golink" href={`/run/${finishedRun.id}`}>See what happened</a>
        </Hint>
      </div>
    </div>
  {/if}

  <!-- It stopped to ask something.
       A question is not a fault, and the page it used to land on was about
       what went wrong. It gets a box instead: the question, somewhere to type,
       and the job done again with the answer. -->
  {#if askedRun}
    <div class="card asked">
      <strong>It needs to know something</strong>
      <p style="margin:6px 0 10px; max-width:62ch">{askedRun.failure?.plain_reason}</p>
      <textarea class="directive" rows="2" bind:value={draftAnswer} placeholder="Your answer" data-hint-exempt="your answer to the question above; nothing happens until you send it"></textarea>
      <div class="row" style="margin-top:8px">
        <Hint id="task.answer">
          <button class="primary" disabled={busy || !draftAnswer.trim()} onclick={sendAnswer}>
            Answer and try again
          </button>
        </Hint>
      </div>
      <p class="muted" style="margin:8px 0 0; max-width:62ch">
        Never a password or a card number. Errand will not ask for one, and this box is not the
        place for one.
      </p>
    </div>
  {/if}

  <!-- One button that does the job.
       There used to be two here for a task that had never run, "Teach it once,
       for real" beside "Teach it as a rehearsal", and the job would then stop
       and wait for somebody to read a document and approve it. Teaching was
       never a different kind of run: it does the job for real and writes down
       how afterwards. So it is just doing the job, and what it writes down
       becomes the way the job is done. -->
  <div class="row" style="flex-wrap:wrap; gap:8px">
    <Hint id="task.run_now">
      <button class="primary" disabled={busy} onclick={() => act(() => api.run(id))}>
        {plan?.active ? "Run it now" : "Do it now"}
      </button>
    </Hint>
    <Hint id="task.dry_run">
      <button disabled={busy} onclick={() => act(() => api.run(id, true))}>Rehearse it first</button>
    </Hint>
    {#if canBeScheduled}
      <Hint id="task.activate">
        <button disabled={busy} onclick={() => act(() => api.activate(id))}>
          {task.schedule_describes ? `Run it ${scheduleClause}` : "Put it on a schedule"}
        </button>
      </Hint>
    {/if}
    <Hint id="task.remove">
      <button disabled={busy} onclick={removeThis}>Get rid of it</button>
    </Hint>
    {#if task.status === "paused"}
      <Hint id="task.resume">
        <button disabled={busy} onclick={() => act(() => api.resume(id))}>Resume</button>
      </Hint>
    {:else if task.status === "ready"}
      <Hint id="task.pause">
        <button disabled={busy} onclick={() => act(() => api.pause(id))}>Pause</button>
      </Hint>
    {/if}
  </div>

  <!-- Said next to the two buttons rather than in the tooltip on one of them.
       The first run of a task is the only run nobody has watched before, and
       until now it was always for real, so which button to press is the whole
       decision being made here. -->
  {#if !hasEverRun}
    <p class="muted" style="margin:8px 0 0; max-width:62ch">
      {#if knownIrreversible}
        This job can do something it cannot take back, so it is worth rehearsing once first: a
        rehearsal goes through the whole thing and records anything irreversible instead of doing
        it.
      {:else}
        It works the job out and does it. If it goes wrong it tries another way, and it stops
        itself rather than going round for ever.
      {/if}
    </p>
  {/if}

  {#if task.auto_paused}
    <div class="err" style="margin-top:16px">
      <h3>This task needs you</h3>
      <div>Errand paused it: {task.paused_reason}</div>
      {#if (task.open_holds ?? 0) > 0}
        <p class="muted" style="margin:8px 0">
          A run began something that cannot be undone and stopped before confirming it. Check the
          site, then tell Errand what you found.
        </p>
        <div class="row">
          <Hint id="hold.resolve">
            <button disabled={busy} onclick={() => act(() => api.resolveHold(id, "did_not_happen"))}>
              It did not happen
            </button>
          </Hint>
          <Hint id="hold.resolve">
            <button disabled={busy} onclick={() => act(() => api.resolveHold(id, "already_happened"))}>
              It already happened
            </button>
          </Hint>
        </div>
      {/if}
    </div>
  {/if}

  <!-- Everything below is settings, and settings are not what this page is for.
       A task exists to produce an outcome; the panels that used to sit between
       the answer and the history were a wall to read past. They are all still
       here, unchanged, one click away, for the times somebody really does want
       to change the schedule or take a permission back. -->
  {#if showSettings}
  <h2 bind:this={settingsTop} class="settings-top">When it runs</h2>
  <div class="card">
    {#if editingSchedule}
      <ScheduleEditor bind:value={draftSchedule} />
      <div class="row" style="margin-top:12px; gap:6px">
        <Hint id="task.save_schedule">
          <button class="primary" disabled={busy} onclick={saveSchedule}>Save the schedule</button>
        </Hint>
        <Hint id="task.cancel_edit">
          <button disabled={busy} onclick={() => (editingSchedule = false)}>Never mind</button>
        </Hint>
      </div>
    {:else}
      <div class="row spread">
        <div>
          <div>{task.schedule_describes ?? "Only when you ask."}</div>
          {#if task.schedule_preview?.length}
            <div class="muted" style="margin-top:4px">
              Next: {task.schedule_preview.map((t) => new Date(t).toLocaleString()).join(" · ")}
            </div>
          {/if}
        </div>
        <Hint id="task.edit_schedule">
          <button disabled={busy} onclick={() => { draftSchedule = task!.schedule ?? { kind: "manual" }; editingSchedule = true; }}>
            Change
          </button>
        </Hint>
      </div>
    {/if}
  </div>

  <h2>What it is allowed to do</h2>
  <div class="card">
    <Hint id="task.allowed_sites"><strong>Sites it may open</strong></Hint>
    {#if editingSites}
      <div style="margin-top:8px">
        <SitesEditor bind:sites={draftSites} />
      </div>
      <div class="row" style="margin-top:12px; gap:6px">
        <Hint id="task.save_sites">
          <button class="primary" disabled={busy} onclick={saveSites}>Save the sites</button>
        </Hint>
        <Hint id="task.cancel_edit">
          <button disabled={busy} onclick={() => (editingSites = false)}>Never mind</button>
        </Hint>
      </div>
    {:else}
      <div class="row spread" style="margin-top:6px">
        <span class="muted">
          {task.allowed_domains?.length
            ? task.allowed_domains.join(", ")
            : "none yet, so it cannot browse at all"}
        </span>
        <Hint id="task.edit_sites">
          <button disabled={busy} onclick={() => { draftSites = [...(task!.allowed_domains ?? [])]; editingSites = true; }}>
            Change
          </button>
        </Hint>
      </div>
    {/if}

    <div class="row spread" style="margin-top:14px">
      <Hint id="task.limits"><span>Limits on a single run</span></Hint>
      {#if !editingLimits}
        <div class="row" style="gap:8px">
          <span class="muted">
            {task.limits
              ? `at most ${task.limits.max_steps} steps, ${task.limits.max_minutes} minutes, $${task.limits.max_usd} of AI, ${task.limits.max_messages} messages` +
                (task.limits.max_spend_usd
                  ? `, and may spend up to $${task.limits.max_spend_usd}`
                  : ", and may not spend money")
              : "stops if it runs too long or spends too much"}
          </span>
          <Hint id="task.edit_limits">
            <button disabled={busy} onclick={startEditLimits}>Change</button>
          </Hint>
        </div>
      {/if}
    </div>
    {#if editingLimits}
      <div class="limits-form">
        <label for="lim-steps">Most things it may do in one run</label>
        <input id="lim-steps" type="number" min="0" bind:value={draftLimits.max_steps} />
        <label for="lim-min">Most minutes one run may take</label>
        <input id="lim-min" type="number" min="0" bind:value={draftLimits.max_minutes} />
        <label for="lim-usd">Most one run may cost you in AI</label>
        <input id="lim-usd" type="number" min="0" step="0.1" bind:value={draftLimits.max_usd} />
        <label for="lim-msg">Most messages one run may send</label>
        <input id="lim-msg" type="number" min="0" bind:value={draftLimits.max_messages} />
        <label for="lim-spend">Most real money one run may spend</label>
        <input id="lim-spend" type="number" min="0" step="1" bind:value={draftLimits.max_spend_usd} />
        <p class="muted" style="margin:6px 0 0">
          The last one is not like the others. It is money leaving your account, and zero means
          this task may not buy anything at all. Nothing is bought without it reading the total off
          the page first, and the number is for the whole run, not per item.
        </p>
        <p class="muted" style="margin:6px 0 0">
          On the rest, a zero means no ceiling. A run that hits a ceiling stops and says which one.
        </p>
      </div>
      <div class="row" style="margin-top:12px; gap:6px">
        <Hint id="task.save_limits">
          <button class="primary" disabled={busy} onclick={saveLimits}>Save the limits</button>
        </Hint>
        <Hint id="task.cancel_edit">
          <button disabled={busy} onclick={() => (editingLimits = false)}>Never mind</button>
        </Hint>
      </div>
    {/if}
  </div>

  <!-- Left off the page entirely when the daemon would not say what models it
       has, rather than drawn as an empty menu somebody cannot act on. -->
  {#if ai}
    <h2>Which AI does this task</h2>
    <div class="card">
      <Hint id="task.model">
        <select
          disabled={busy}
          value={task.model_id ?? ""}
          onchange={(e) => chooseModel((e.currentTarget as HTMLSelectElement).value)}
        >
          <option value="">
            {defaultModel ? `Default (currently ${defaultModel})` : "Default"}
          </option>
          {#each ai.providers as p}
            <!-- A model Errand has found wanting is shown and not offered. It
                 stays in the list because a name missing altogether reads as a
                 bug, and the reason is said underneath. A model that is merely
                 switched off can still be picked, the same as on the AI screen:
                 it is a model you have, not one that cannot do the job. -->
            <option value={p.id} disabled={!!p.cannot_carry_out_because}>
              {p.display_name}{modelNote(p)}
            </option>
          {/each}
        </select>
      </Hint>

      <p class="muted" style="margin:8px 0 0; max-width:62ch">
        {#if task.model_id && !taskModel}
          <!-- The menu cannot show a model that is not in it, so it falls back
               to reading "Default", which is what will happen but not what the
               task says. Saying so beats letting the menu be believed. -->
          This task asks for a model that is no longer in Errand's list, so it runs on the
          default until you pick another one.
        {:else if taskModel}
          This task uses {taskModel.display_name}, whatever the AI screen is set to.
        {:else if defaultModel}
          This task follows the AI screen, which is set to {defaultModel}. Choosing one here
          changes it for this task only.
        {:else}
          This task follows whatever the AI screen is set to. Choosing one here changes it for
          this task only.
        {/if}
      </p>

      {#if readsSomethingPrivate}
        <!-- Said here and only here: the model carrying the task out is the one
             that reads the mail, so this is where that choice has consequences
             somebody would mind. -->
        <p class="muted" style="margin:8px 0 0; max-width:62ch">
          This task can read your mail, and the model doing the job is what reads it. A model on
          your own machine keeps it here.
        </p>
      {/if}

      {#each ai.providers.filter((p) => p.cannot_carry_out_because) as p}
        <p class="muted" style="margin:8px 0 0; max-width:62ch">{p.cannot_carry_out_because}</p>
      {/each}
    </div>
  {/if}

  <h2>Who it tells when it is done</h2>
  <div class="card">
    {#if people.length === 0}
      <p class="muted" style="margin:0 0 8px">
        Nobody. You will still get the result yourself on Telegram if you have set that up. Adding
        someone here is what lets a finished job send them a message.
      </p>
    {:else}
      {#each people as p}
        <div class="row spread" style="margin-bottom:6px">
          <div>
            <strong>{p.label}</strong>
            <span class="muted"> · {named(p.channel)} · {p.address}</span>
          </div>
          <div class="row" style="gap:6px">
            <Hint id="task.notify_when">
              <button
                class="pill toggle {p.on_success ? 'ok' : ''}"
                disabled={busy}
                onclick={() => togglePerson(p, "success")}
              >{p.on_success ? "when it works" : "not on success"}</button>
            </Hint>
            <Hint id="task.notify_when">
              <button
                class="pill toggle {p.on_failure ? 'warn' : ''}"
                disabled={busy}
                onclick={() => togglePerson(p, "failure")}
              >{p.on_failure ? "when it fails" : "not on failure"}</button>
            </Hint>
            <Hint id="task.unlink_person">
              <button disabled={busy} onclick={() => act(() => api.unlinkRecipient(id, p.id))}>Remove</button>
            </Hint>
          </div>
        </div>
      {/each}
    {/if}

    {#if tooManyPeople}
      <div class="warnbox" style="margin-top:10px">
        This task may send {task.limits!.max_messages} message(s) per run but {people.length}
        people are waiting to hear from it, so some of them will not be told. The run says who.
        Raise the message limit if that is not what you want.
      </div>
    {/if}

    {#if unlinked.length}
      <div class="row" style="gap:6px; margin-top:10px; flex-wrap:wrap">
        <Hint id="task.link_person">
          <select
            disabled={busy}
            onchange={(e) => {
              const v = (e.currentTarget as HTMLSelectElement).value;
              if (v) act(() => api.linkRecipient(id, v, true, true));
              (e.currentTarget as HTMLSelectElement).value = "";
            }}
          >
            <option value="">Tell someone when this finishes…</option>
            {#each unlinked as r}
              <option value={r.id}>{r.label} · {named(r.channel)}</option>
            {/each}
          </select>
        </Hint>
      </div>
    {:else if everyone.length === 0}
      <p class="muted" style="margin:8px 0 0">
        You have not saved anyone yet. Add people in Settings first, then they can be picked here.
      </p>
    {/if}
  </div>

  <!-- The most personal permission in the app, so the screen that grants it
       says where the mail actually goes before it offers the button. The
       sentence itself comes from the daemon, because whether your post leaves
       this Mac depends on which model is doing the job, and the daemon is what
       knows that. -->
  {#if mail}
    <h2>Reading your mail</h2>
    <div class="card">
      <p class="muted" style="margin:0 0 10px">
        Switched on, this task can see what is in a mailbox, who each message is from and what it
        is about, and can open individual messages. It can never send a message, reply to one, or
        delete one. Moving messages, which is how spam gets tidied into Junk, is a separate
        answer below.
      </p>

      <div class="warnbox" style="margin:0 0 12px">{mail.where_it_goes}</div>

      {#if mail.granted}
        <div class="row spread">
          <div>
            <strong>This task can read your mail.</strong>
            <span class="muted"> · every message it opens is written into the run, by who it was
              from and what it was about, and never its contents</span>
          </div>
          <div class="row" style="gap:6px">
            <Hint id="task.mail_file">
              <button
                class="pill toggle {mail.may_file ? 'warn' : ''}"
                disabled={busy}
                onclick={() => setMailAccess(!mail!.may_file)}
              >{mail.may_file ? "may move messages" : "cannot move messages"}</button>
            </Hint>
            <Hint id="task.mail_revoke">
              <button disabled={busy} onclick={() => act(() => api.revokeMail(id))}>Take the mail away</button>
            </Hint>
          </div>
        </div>
      {:else}
        <Hint id="task.mail_grant">
          <button disabled={busy} onclick={() => setMailAccess(false)}>Let this task read my mail</button>
        </Hint>
      {/if}
    </div>
  {/if}

  <!-- How it does the job: mechanics, and mechanics belong behind the gear.
       It is here rather than on the front of the page because a person opens a
       task to see what it produced. This is where to look when the outcome was
       not what they meant and they want to change how it is done rather than
       what they asked for. -->
  <h2>How it does this job</h2>
  {#if plan?.active}
    <Hint id="playbook.what">
      <span class="pill ok">Approved, version {plan.active.version}</span>
    </Hint>
    <pre class="card" style="white-space:pre-wrap; font-size:12.5px; margin-top:8px">{plan.active.markdown}</pre>
  {:else if plan?.versions?.length}
    <div class="card">
      <strong>Waiting for you to read it</strong>
      <p class="muted" style="margin:6px 0 10px">{plan.note}</p>
      {#each plan.versions as v}
        <div class="row spread" style="margin-top:8px">
          <span>Version {v.version} <span class="muted">from {v.source}</span></span>
          <Hint id="playbook.approve">
            <button class="primary" disabled={busy}
              onclick={() => act(() => api.approvePlaybook(id, v.version))}>Approve</button>
          </Hint>
        </div>
        <!-- What the run said about itself when it wrote this. A plan written
             by a rehearsal reads exactly like one written by a run that really
             did the job, and that difference is the whole of what is being
             approved. -->
        {#if v.changelog}
          <p class="muted" style="margin:4px 0 0; max-width:70ch">{v.changelog}</p>
        {/if}
        <!-- The plan itself, which is the thing the Approve button above is
             asking about. Folded away rather than absent: several rehearsals
             leave several versions waiting, and three plans printed in full
             would bury the buttons. Open by default when there is only one,
             because then there is nothing to bury. -->
        {#if v.markdown}
          <details open={plan.versions.length === 1} style="margin-top:6px">
            <summary class="muted" data-hint-exempt="shows the plan this Approve button is about; nothing happens on the machine"
              style="cursor:pointer">Read version {v.version}</summary>
            <pre class="card" style="white-space:pre-wrap; font-size:12.5px; margin-top:6px">{v.markdown}</pre>
          </details>
        {/if}
      {/each}
    </div>
  {:else}
    <div class="card">
      <p class="muted" style="margin:0">
        Nothing yet. It writes this down the first time it does the job, and follows it after
        that.
      </p>
    </div>
  {/if}
  {/if}

  <h2>Runs</h2>
  <!-- Guarded, like the list on the front page: "It has not run yet" is a
       sentence about the history being empty, and printing it when the history
       could not be read is a lie that reads before the banner does. -->
  {#if runs.length === 0 && !problem}
    <p class="muted">It has not run yet.</p>
  {:else}
    {#each runs.slice(0, 12) as r}
      <a class="plain" href={`/run/${r.id}`} data-hint-exempt="opens the run shown in this card">
        <div class="card">
          <div class="row spread">
            <div>
              <span class="pill {r.status === 'succeeded' ? 'ok' : r.status === 'failed' ? 'bad' : ''}">
                {statusLabel(r.status)}
              </span>
              {#if r.rehearsal}<span class="pill warn">rehearsal</span>{/if}
              <div class="muted" style="margin-top:6px">
                {(r.summary ?? r.failure?.plain_reason ?? "").split("\n")[0].slice(0, 110)}
              </div>
            </div>
            <span class="muted">{when(r.created_at)}</span>
          </div>
        </div>
      </a>
    {/each}
  {/if}
{:else if !problem}
  <p class="muted">Loading…</p>
{/if}

<style>
  /* Not the red of a failure. It is a question, and the page should not look
     like something broke. */
  .asked { border-left: 3px solid var(--accent, var(--rule)); }

  .directive {
    width: 100%;
    font: inherit;
    line-height: 1.5;
  }
  /* A control that reads as part of the sentence it follows, because that is
     what it is: a way to change the words above it. */
  .linkish {
    background: none;
    border: 0;
    padding: 0 0 0 6px;
    color: var(--ink-soft);
    text-decoration: underline;
    font-size: 13px;
    cursor: pointer;
  }
  .linkish:hover { color: var(--ink); }

  /* A quiet control. It sits beside the status because that is where the eye
     already is when the page opens, and it must not compete with the answer. */
  .gear {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 30px;
    padding: 0;
    border-radius: 7px;
    color: var(--ink-soft);
    background: transparent;
    border: 1px solid transparent;
  }
  .gear:hover, .gear[aria-expanded="true"] {
    color: var(--ink);
    background: var(--surface-2);
    border-color: var(--rule);
  }

  /* Room above the heading the gear jumps to, so it lands as the top of a
     section rather than flush against the window edge, where it reads as the
     page having been cut off. */
  .settings-top { scroll-margin-top: 16px; }

  /* The answer, set as something to read rather than a status line.
     pre-wrap rather than a Markdown renderer: this text is written by a model
     and often quotes a web page, and putting that through {@html} inside the
     app's own webview buys formatting with a class of bug nobody wants. The
     playbook is shown the same way. */
  .answer {
    white-space: pre-wrap;
    color: var(--ink);
    font-size: 14px;
    line-height: 1.55;
    max-width: 72ch;
  }

  /* This page had no styles of its own; app.css covers the rest. */
  .warnbox {
    background: var(--warn-bg); color: var(--warn);
    padding: 9px 11px; border-radius: 6px; font-size: 12.5px;
    display: flex; flex-direction: column; gap: 4px; margin: 12px 0;
  }
  /* A pill that is also a button: the notify flags switch on click, so they
     must look pressable rather than purely informative. */
  .pill.toggle { cursor: pointer; border: 1px solid var(--rule); background: transparent; }
  .pill.toggle:disabled { cursor: default; opacity: 0.5; }
  .limits-form { display: grid; gap: 4px; margin-top: 10px; max-width: 340px; }
  .limits-form input { width: 110px; }

  /* The two cards at the top of the page. A rule down the side, so which of the
     two you are looking at is answered before you have read a word. */
  .now { border-left: 3px solid var(--accent); }
  .ended { border-left: 3px solid var(--ok); }
  .ended.bad { border-left-color: var(--bad); }
  /* A link that behaves like a button, because getting to the run is the point
     of both cards and a plain underline is easy to miss. */
  .golink {
    display: inline-block; padding: 6px 12px; border-radius: 6px;
    border: 1px solid var(--rule); background: var(--surface-2);
    color: var(--ink); text-decoration: none; white-space: nowrap;
  }
  .golink:hover { border-color: var(--accent); }
  /* Something on the page has to be moving, or "working on it" is just a claim. */
  .blip {
    display: inline-block; width: 7px; height: 7px; border-radius: 50%;
    background: var(--accent); margin-right: 7px; vertical-align: middle;
    animation: blip 1.4s ease-in-out infinite;
  }
  @keyframes blip { 50% { opacity: 0.25; } }
  @media (prefers-reduced-motion: reduce) { .blip { animation: none; } }
</style>
