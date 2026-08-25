<!--
  Choosing when a task runs.

  Nobody should have to write a cron expression, so this builds one. But a form
  that builds a schedule can be wrong in ways nobody notices until the run does
  not happen, so it never trusts itself: every change is sent to the daemon,
  which says back what it will REALLY do and when the next three runs are. The
  sentence under the form comes from the engine, not from this file. If the two
  ever disagree, you find out here rather than at eight next Tuesday.

  Note for anyone editing the cron strings: the engine takes SIX fields, seconds
  first. A five-field expression copied from a website means something else
  entirely here.
-->
<script lang="ts">
  import { api, type SchedulePreview } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";

  let { value = $bindable() }: { value: any } = $props();

  const WEEKDAYS = [
    ["MON", "Monday"], ["TUE", "Tuesday"], ["WED", "Wednesday"], ["THU", "Thursday"],
    ["FRI", "Friday"], ["SAT", "Saturday"], ["SUN", "Sunday"],
  ] as const;

  // The timezone list comes from the browser, so it always matches what this
  // Mac actually knows about rather than a list that rots in the source.
  const ZONES: string[] = (() => {
    try {
      // @ts-expect-error - supportedValuesOf is newer than the DOM types here
      return Intl.supportedValuesOf("timeZone");
    } catch {
      return [Intl.DateTimeFormat().resolvedOptions().timeZone, "UTC"];
    }
  })();

  type Every = "manual" | "once" | "day" | "week" | "month" | "advanced";

  // Read the existing schedule back into the form. Anything this cannot express
  // opens in Advanced with the real expression showing, rather than being
  // quietly rewritten into something simpler that means something different.
  function readBack(spec: any): { every: Every; time: string; dow: string; dom: number; expr: string; at: string } {
    const out = { every: "manual" as Every, time: "08:00", dow: "MON", dom: 1, expr: "", at: "" };
    if (!spec || spec.kind === "manual") return out;
    if (spec.kind === "once") return { ...out, every: "once", at: spec.at ?? "" };
    if (spec.kind !== "cron") return out;

    out.expr = spec.expr ?? "";
    const f = out.expr.split(/\s+/);
    if (f.length !== 6 || f[0] !== "0" || f[4] !== "*") return { ...out, every: "advanced" };
    const m = Number(f[1]), h = Number(f[2]);
    if (!Number.isInteger(m) || !Number.isInteger(h) || m > 59 || h > 23) {
      return { ...out, every: "advanced" };
    }
    out.time = `${String(h).padStart(2, "0")}:${String(m).padStart(2, "0")}`;
    if (f[3] === "*" && f[5] === "*") return { ...out, every: "day" };
    if (f[3] === "*" && WEEKDAYS.some(([k]) => k === f[5].toUpperCase())) {
      return { ...out, every: "week", dow: f[5].toUpperCase() };
    }
    if (f[5] === "*" && Number(f[3]) >= 1 && Number(f[3]) <= 31) {
      return { ...out, every: "month", dom: Number(f[3]) };
    }
    return { ...out, every: "advanced" };
  }

  const initial = readBack(value);
  let every = $state<Every>(initial.every);
  let time = $state(initial.time);
  let dow = $state<string>(initial.dow);
  let dom = $state(initial.dom);
  let expr = $state(initial.expr);
  let at = $state(initial.at);
  let tz = $state(value?.tz ?? Intl.DateTimeFormat().resolvedOptions().timeZone ?? "UTC");
  let catchUp = $state(value?.catch_up ?? "run_once_late");
  let jitter = $state<number>(value?.jitter_s ?? 0);
  let showAdvanced = $state(initial.every === "advanced");

  let preview = $state<SchedulePreview | null>(null);
  let checking = $state(false);

  function build(): any {
    const [h, m] = time.split(":");
    const hh = Number(h) || 0, mm = Number(m) || 0;
    const common = { tz, catch_up: catchUp, jitter_s: Number(jitter) || 0 };
    switch (every) {
      case "manual": return { kind: "manual", ...common };
      case "once": return { kind: "once", at, ...common };
      // Six fields, seconds first.
      case "day": return { kind: "cron", expr: `0 ${mm} ${hh} * * *`, ...common };
      case "week": return { kind: "cron", expr: `0 ${mm} ${hh} * * ${dow}`, ...common };
      case "month": return { kind: "cron", expr: `0 ${mm} ${hh} ${dom} * *`, ...common };
      case "advanced": return { kind: "cron", expr, ...common };
    }
  }

  // Debounced, because this asks the daemon on every keystroke otherwise.
  let timer: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    const spec = build();
    value = spec;
    clearTimeout(timer);
    checking = true;
    timer = setTimeout(async () => {
      try { preview = await api.previewSchedule(spec); }
      catch (e) { preview = { valid: false, describes: "", preview: [], problem: String(e) }; }
      finally { checking = false; }
    }, 250);
  });

  const local = (iso: string) =>
    new Date(iso).toLocaleString(undefined, {
      weekday: "short", day: "numeric", month: "short",
      hour: "2-digit", minute: "2-digit",
    });
</script>

<div class="sched">
  <div class="grid">
    <label for="sched-every">How often</label>
    <Hint id="sched.every">
      <select id="sched-every" bind:value={every}>
        <option value="manual">Only when I ask</option>
        <option value="once">Once, at a set time</option>
        <option value="day">Every day</option>
        <option value="week">Every week</option>
        <option value="month">Every month</option>
        <option value="advanced">Something more specific</option>
      </select>
    </Hint>

    {#if every === "once"}
      <label for="sched-at">When</label>
      <input id="sched-at" type="datetime-local" bind:value={at} step="60" />
    {/if}

    {#if every === "week"}
      <label for="sched-dow">Day</label>
      <select id="sched-dow" bind:value={dow}>
        {#each WEEKDAYS as [key, name]}<option value={key}>{name}</option>{/each}
      </select>
    {/if}

    {#if every === "month"}
      <label for="sched-dom">Day of the month</label>
      <input id="sched-dom" type="number" min="1" max="31" bind:value={dom} />
    {/if}

    {#if every === "day" || every === "week" || every === "month"}
      <label for="sched-time">At</label>
      <input id="sched-time" type="time" bind:value={time} step="60" />
    {/if}

    {#if every === "advanced"}
      <label for="sched-expr">Schedule expression</label>
      <Hint id="sched.expr">
        <input id="sched-expr" bind:value={expr} placeholder="0 0 8 * * MON-FRI" spellcheck="false" />
      </Hint>
    {/if}

    {#if every !== "manual"}
      <label for="sched-tz">Time zone</label>
      <Hint id="sched.tz">
        <select id="sched-tz" bind:value={tz}>
          {#each ZONES as z}<option value={z}>{z}</option>{/each}
        </select>
      </Hint>
    {/if}
  </div>

  <!-- The engine's own answer, not this form's. -->
  {#if every !== "manual"}
    <div class="answer" class:bad={preview && !preview.valid}>
      {#if checking && !preview}
        <span class="muted">Checking…</span>
      {:else if preview && !preview.valid}
        <strong>That will not work.</strong> {preview.problem}
      {:else if preview}
        <div>{preview.describes}</div>
        {#if preview.preview.length}
          <div class="muted next">
            Next: {preview.preview.map(local).join(" · ")}
          </div>
        {:else}
          <div class="muted next">It will not run again.</div>
        {/if}
      {/if}
    </div>
  {/if}

  {#if every !== "manual"}
    <Hint id="sched.more">
      <button type="button" class="linky" onclick={() => (showAdvanced = !showAdvanced)}>
        {showAdvanced ? "Hide the fine print" : "Missed runs, and spreading the load"}
      </button>
    </Hint>

    {#if showAdvanced}
      <div class="grid">
        <label for="sched-catchup">If your Mac was asleep</label>
        <Hint id="sched.catch_up">
          <select id="sched-catchup" bind:value={catchUp}>
            <option value="run_once_late">Run it once, late</option>
            <option value="skip">Skip it</option>
            <option value="run_all">Make up every missed run</option>
          </select>
        </Hint>

        <label for="sched-jitter">Start within</label>
        <Hint id="sched.jitter">
          <select id="sched-jitter" bind:value={jitter}>
            <option value={0}>Exactly on time</option>
            <option value={60}>A minute either way</option>
            <option value={300}>Five minutes either way</option>
            <option value={900}>Fifteen minutes either way</option>
          </select>
        </Hint>
      </div>
    {/if}
  {/if}
</div>

<style>
  .sched { display: flex; flex-direction: column; gap: 10px; }
  .grid {
    display: grid; grid-template-columns: minmax(120px, auto) 1fr;
    gap: 8px 12px; align-items: center; max-width: 520px;
  }
  .grid label { font-size: 12px; color: var(--ink-faint); }
  .answer {
    background: var(--ok-bg); color: var(--ok);
    padding: 9px 11px; border-radius: 6px; font-size: 12.5px; max-width: 520px;
  }
  .answer.bad { background: var(--bad-bg); color: var(--bad); }
  .next { margin-top: 4px; opacity: 0.85; }
  .linky {
    background: none; border: none; padding: 0; color: var(--accent);
    cursor: pointer; font-size: 12.5px; text-align: left;
  }
</style>
