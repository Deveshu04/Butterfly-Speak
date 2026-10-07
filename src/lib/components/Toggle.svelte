<script lang="ts">
  let {
    label,
    description = "",
    checked = $bindable(false),
    disabled = false,
    onchange = () => {},
  }: {
    label: string;
    description?: string;
    checked?: boolean;
    disabled?: boolean;
    onchange?: (value: boolean) => void | Promise<void>;
  } = $props();

  // If persisting the change fails, flip the switch back — otherwise the UI
  // shows a state the backend never accepted.
  async function handleChange() {
    try {
      await onchange(checked);
    } catch (e) {
      console.error("toggle change failed:", e);
      checked = !checked;
    }
  }
</script>

<label class="row" class:disabled>
  <div class="text">
    <span class="label">{label}</span>
    {#if description}
      <span class="description">{description}</span>
    {/if}
  </div>
  <input type="checkbox" bind:checked {disabled} onchange={handleChange} />
  <span class="switch" aria-hidden="true"></span>
</label>

<style>
  .row {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 12px 0;
    cursor: pointer;
  }

  .row.disabled {
    opacity: 0.45;
    cursor: default;
  }

  .text {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .label {
    font-weight: 500;
  }

  .description {
    color: var(--fg-muted);
    font-size: 13px;
    line-height: 1.45;
  }

  input {
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }

  .switch {
    flex: none;
    width: 36px;
    height: 20px;
    border-radius: 999px;
    border: 1px solid var(--hairline);
    background: var(--switch-track);
    position: relative;
    transition: background var(--motion), border-color var(--motion);
  }

  .switch::after {
    content: "";
    position: absolute;
    top: 2px;
    left: 2px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--fg-muted);
    transition: transform var(--motion), background var(--motion);
  }

  input:checked + .switch {
    background: var(--accent);
    border-color: var(--accent);
  }

  input:checked + .switch::after {
    transform: translateX(16px);
    background: var(--accent-fg);
  }

  input:focus-visible + .switch {
    box-shadow: var(--focus-ring);
  }
</style>
