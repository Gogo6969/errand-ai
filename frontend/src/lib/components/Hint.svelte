<script lang="ts">
  /**
   * Wraps a control and explains it.
   *
   * Every interactive thing in the app is wrapped in one of these, and an audit
   * script fails the build if one is not. The same text becomes the tooltip and
   * the accessible description, so a screen reader and a mouse get the same
   * answer rather than two different ones.
   */
  import { hints, type HintId } from "$lib/hints";
  let { id, children }: { id: HintId; children: any } = $props();
  const h = hints[id];
  let open = $state(false);
</script>

<span class="hint" onmouseenter={() => (open = true)} onmouseleave={() => (open = false)}>
  <span aria-describedby={`hint-${id}`}>{@render children()}</span>
  <span id={`hint-${id}`} class="bubble" class:open role="tooltip">
    {h.short}
    {#if h.long}<span class="more">{h.long}</span>{/if}
  </span>
</span>

<style>
  .hint { position: relative; display: inline-flex; }
  .bubble {
    position: absolute; bottom: calc(100% + 8px); left: 0; z-index: 50;
    width: max-content; max-width: 22rem;
    background: var(--surface-2); color: var(--ink);
    border: 1px solid var(--rule); border-radius: 6px;
    padding: 8px 10px; font-size: 12.5px; line-height: 1.45;
    box-shadow: 0 8px 24px rgb(0 0 0 / 0.18);
    opacity: 0; visibility: hidden; transition: opacity .12s;
    pointer-events: none; text-align: left; font-weight: 400;
  }
  .bubble.open { opacity: 1; visibility: visible; }
  .more { display: block; margin-top: 6px; color: var(--ink-soft); }
</style>
