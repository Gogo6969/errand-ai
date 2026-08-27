<script lang="ts">
  /**
   * What a page says when something went wrong, and how it gets back.
   *
   * One component so every screen says it the same way and offers the same way
   * out. Before this, a page that could not reach the background service left
   * a red banner and nothing to press.
   */
  import Hint from "$lib/components/Hint.svelte";

  let {
    problem,
    retrying = false,
    onRetry,
  }: { problem: string; retrying?: boolean; onRetry?: () => void } = $props();
</script>

<div class="err">
  <h3>Something is not right</h3>
  <div>{problem}</div>
  {#if onRetry}
    <div class="row" style="margin-top:10px; align-items:center; gap:10px">
      <Hint id="app.retry">
        <button disabled={retrying} onclick={onRetry}>
          {retrying ? "Trying again…" : "Try again"}
        </button>
      </Hint>
      {#if retrying}
        <span class="muted">It usually comes back on its own.</span>
      {/if}
    </div>
  {/if}
</div>
