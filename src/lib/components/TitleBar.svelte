<script lang="ts">
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import Icon from "./Icon.svelte";

  const win = getCurrentWindow();
</script>

<header class="titlebar" data-tauri-drag-region>
  <div class="controls">
    <button aria-label="Minimize" onclick={() => win.minimize()}>
      <Icon name="minimize" size={16} stroke={1.5} />
    </button>
    <button aria-label="Maximize" onclick={() => win.toggleMaximize()}>
      <Icon name="maximize" size={14} stroke={1.5} />
    </button>
    <button class="close" aria-label="Close" onclick={() => win.close()}>
      <Icon name="close" size={15} stroke={1.5} />
    </button>
  </div>
</header>

<style>
  .titlebar {
    height: var(--titlebar-h);
    flex: none;
    display: flex;
    align-items: flex-start;
    justify-content: flex-end;
    background: transparent;
  }

  .controls {
    display: flex;
  }

  button {
    width: 46px;
    height: 34px;
    display: grid;
    place-items: center;
    border: none;
    background: transparent;
    color: var(--fg-muted);
    cursor: default;
    transition: background var(--motion), color var(--motion);
  }

  button:hover {
    background: var(--wash-strong);
    color: var(--fg);
  }

  /* Not tokenized on purpose: this is the Windows close-button red, and it is
     the same red in a light and a dark Explorer window. Theming it would make
     the one control users hit by muscle memory stop looking like the OS. */
  button.close:hover {
    background: #e81123;
    color: #ffffff;
  }
</style>
