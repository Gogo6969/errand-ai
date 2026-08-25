<script lang="ts">
  import { page } from "$app/state";
  import "../app.css";
  let { children } = $props();
  const nav = [
    { href: "/", label: "Tasks" },
    { href: "/runs", label: "History" },
    { href: "/ai", label: "AI" },
    { href: "/settings", label: "Settings" },
  ];
</script>

<div class="shell">
  <nav>
    <span class="brand">Errand<span class="dim">-AI</span></span>
    {#each nav as n}
      <a href={n.href} class:active={page.url.pathname === n.href} data-hint-exempt="navigation, labelled by its own text">{n.label}</a>
    {/each}
  </nav>
  <main>{@render children()}</main>
</div>
