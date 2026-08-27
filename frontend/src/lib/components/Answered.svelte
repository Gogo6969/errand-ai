<script lang="ts">
  /**
   * A run's answer, with anything in it that can be opened made openable.
   *
   * The answer used to be printed as plain text, so a link in it was a piece of
   * string. Somebody asked a task to put a link to each message beside it "so
   * that I can open it here", got a page of unclickable text, and was told by
   * the run that Mail has no link for a message. Mail does: macOS gives it the
   * `message:` scheme. Both halves of that were broken, and this is the half
   * the window owns.
   *
   * Split rather than parsed. The text is a model's prose and the only thing
   * being looked for is a run of characters that starts with a scheme Errand
   * will open, so a regular expression is the whole job. Everything else stays
   * text, and Svelte escapes it, which is what keeps a message subject written
   * by a stranger from becoming markup.
   */
  import { openLink } from "$lib/api";
  import Hint from "$lib/components/Hint.svelte";

  let { text }: { text: string } = $props();

  // The schemes Rust will open, and nothing else is worth marking up.
  const LINK = /\b(?:message|https|http|mailto):\S+/gi;

  const pieces = $derived.by(() => {
    const out: { text: string; link: boolean }[] = [];
    let at = 0;
    for (const m of text.matchAll(LINK)) {
      const start = m.index ?? 0;
      if (start > at) out.push({ text: text.slice(at, start), link: false });
      // Trailing punctuation belongs to the sentence, not to the link.
      const raw = m[0].replace(/[.,;:)\]]+$/, "");
      out.push({ text: raw, link: true });
      at = start + raw.length;
    }
    if (at < text.length) out.push({ text: text.slice(at), link: false });
    return out;
  });

  let problem = $state<string | null>(null);

  async function open(url: string) {
    problem = null;
    try {
      await openLink(url);
    } catch (e) {
      problem = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<div class="answer">{#each pieces as p}{#if p.link}<Hint id="answer.link"><button
        class="linkish"
        onclick={() => open(p.text)}>{shorten(p.text)}</button></Hint>{:else}{p.text}{/if}{/each}</div>
{#if problem}<div class="muted" style="margin-top:6px">{problem}</div>{/if}

<script lang="ts" module>
  /**
   * A message link is thirty characters of percent-encoded message id and says
   * nothing to anybody. The words are what somebody clicks; the address is what
   * the machine needs.
   */
  function shorten(url: string): string {
    if (/^message:/i.test(url)) return "Open in Mail";
    try {
      return new URL(url).host || url;
    } catch {
      return url;
    }
  }
</script>

<style>
  /* Set to be read. pre-wrap rather than rendered Markdown: this text is
     written by a model and often quotes a page, and {@html} over that inside
     the app's own webview buys formatting with a class of bug nobody wants.
     Which is also why the links above are found with a pattern and rendered as
     controls rather than by turning the answer into HTML. */
  .answer {
    white-space: pre-wrap;
    color: var(--ink);
    font-size: 14px;
    line-height: 1.55;
    max-width: 72ch;
  }
  .linkish {
    background: none;
    border: 0;
    padding: 0;
    font: inherit;
    color: var(--accent, #7aa2f7);
    text-decoration: underline;
    cursor: pointer;
  }
</style>
