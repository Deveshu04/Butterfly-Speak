<script lang="ts">
  import type { Snippet } from "svelte";
  import Icon from "./Icon.svelte";

  // A designed empty state — a component, not text floating in whitespace.
  let {
    icon = "note",
    title,
    body = "",
    action,
  }: {
    icon?: string;
    title: string;
    body?: string;
    action?: Snippet;
  } = $props();
</script>

<div class="empty">
  <div class="glyph">
    <Icon name={icon} size={20} stroke={1.5} />
  </div>
  <p class="title">{title}</p>
  {#if body}
    <p class="body">{body}</p>
  {/if}
  {#if action}
    <div class="act">{@render action()}</div>
  {/if}
</div>

<style>
  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    text-align: center;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    background: var(--bg-elevated);
    padding: 40px 28px;
  }

  .glyph {
    width: 44px;
    height: 44px;
    border-radius: 50%;
    border: 1px solid var(--hairline-strong);
    background: var(--surface);
    display: grid;
    place-items: center;
    color: var(--fg-faint);
    margin-bottom: 14px;
  }

  .title {
    font-size: 14.5px;
    font-weight: 600;
    color: var(--fg);
    margin: 0 0 4px;
  }

  .body {
    font-size: 13px;
    line-height: 1.55;
    color: var(--fg-muted);
    margin: 0;
    max-width: 40ch;
  }

  .act {
    margin-top: 16px;
  }

  .act :global(button) {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--accent-fg);
    border-radius: var(--radius-control);
    padding: 8px 18px;
    cursor: pointer;
  }
</style>
