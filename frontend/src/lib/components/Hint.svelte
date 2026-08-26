<script lang="ts">
  /**
   * Wraps a control and explains it.
   *
   * Every interactive thing in the app is wrapped in one of these, and an audit
   * script fails the build if one is not. The same text becomes the tooltip and
   * the accessible description, so a screen reader and a mouse get the same
   * answer rather than two different ones.
   *
   * The bubble is positioned in script rather than in CSS, which is worth the
   * few lines: an absolutely positioned bubble is clipped by the window edge
   * and by any ancestor that scrolls, and a tooltip you can only half read is
   * worse than none at all. Fixed positioning takes it out of every ancestor's
   * clipping box, and the placement below keeps all four edges on screen.
   */
  import { hints, type HintId, type Hint as HintText } from "$lib/hints";

  let { id, children }: { id: HintId; children: any } = $props();
  const h: HintText = $derived(hints[id]);

  let wrap: HTMLElement | undefined = $state();
  let bubble: HTMLElement | undefined = $state();
  let open = $state(false);
  let placed = $state(false);
  let top = $state(0);
  let left = $state(0);

  /** Clear of the window edge, and clear of the control it belongs to. */
  const EDGE = 8;
  const GAP = 8;

  function place() {
    if (!wrap || !bubble) return;
    const t = wrap.getBoundingClientRect();
    const b = bubble.getBoundingClientRect();
    const vw = window.innerWidth;
    const vh = window.innerHeight;

    // Above the control by preference, because that is where the eye already
    // is; below when there is not enough room above.
    let y = t.top - b.height - GAP;
    if (y < EDGE) {
      const below = t.bottom + GAP;
      // Only move below if it genuinely fits better there.
      y = below + b.height + EDGE <= vh ? below : Math.max(EDGE, vh - b.height - EDGE);
    }

    // Start at the control's left edge, then pull it back inside the window.
    // Pulling back beats centring: the text stays where the reader expects it.
    let x = t.left;
    if (x + b.width + EDGE > vw) x = vw - b.width - EDGE;
    if (x < EDGE) x = EDGE;

    top = y;
    left = x;
    placed = true;
  }

  function show() {
    open = true;
    // One frame, so the bubble has been laid out and can be measured.
    requestAnimationFrame(place);
  }

  function hide() {
    open = false;
    placed = false;
  }

  /** Escape closes it, the way it closes anything else that floats. */
  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape" && open) hide();
  }
</script>

<svelte:window
  onscroll={() => open && place()}
  onresize={() => open && place()}
  onkeydown={onKey}
/>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- This wrapper is decoration around a real control, and the hover handlers
     only move a bubble. Nothing here needs a role of its own: the explanation
     reaches assistive technology through aria-describedby on the control
     itself, so a screen reader gets it without any pointer event at all. -->
<span
  class="hint"
  bind:this={wrap}
  onmouseenter={show}
  onmouseleave={hide}
  onfocusin={show}
  onfocusout={hide}
>
  <span aria-describedby={`hint-${id}`}>{@render children()}</span>
  <span
    id={`hint-${id}`}
    class="bubble"
    class:open
    class:placed
    role="tooltip"
    bind:this={bubble}
    style="top:{top}px; left:{left}px"
  >
    {h.short}
    {#if h.long}<span class="more">{h.long}</span>{/if}
  </span>
</span>

<style>
  .hint { display: inline-flex; }
  .bubble {
    position: fixed;
    z-index: 100;
    /* Never wider than the window, however long the sentence. */
    width: max-content;
    max-width: min(22rem, calc(100vw - 16px));
    /* A very long explanation in a short window scrolls rather than spilling
       off the top, which is the other way a tooltip becomes unreadable. */
    max-height: calc(100vh - 16px);
    overflow-y: auto;
    background: var(--surface-2); color: var(--ink);
    border: 1px solid var(--rule); border-radius: 6px;
    padding: 8px 10px; font-size: 12.5px; line-height: 1.45;
    box-shadow: 0 8px 24px rgb(0 0 0 / 0.18);
    opacity: 0; visibility: hidden; transition: opacity 0.12s;
    pointer-events: none; text-align: left; font-weight: 400;
  }
  /* Shown only once it has been measured and moved, so it never appears in the
     top-left corner for a frame before jumping into place. */
  .bubble.open.placed { opacity: 1; visibility: visible; }
  .more { display: block; margin-top: 6px; color: var(--ink-soft); }
  @media (prefers-reduced-motion: reduce) {
    .bubble { transition: none; }
  }
</style>
