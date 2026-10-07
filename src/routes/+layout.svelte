<script lang="ts">
  import { onMount } from "svelte";
  import "@fontsource-variable/inter";
  import "../styles/tokens.css";
  import { theme } from "$lib/theme.svelte";

  let { children } = $props();

  // Both windows go through this layout, so both get the theme wired up: the
  // pill has to follow the setting too, and it has no other entry point.
  // The `.dark` class itself was already stamped before first paint by the
  // inline script in app.html; this only keeps it current afterwards.
  onMount(() => theme.init());
</script>

{@render children()}
