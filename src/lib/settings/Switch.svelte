<script lang="ts">
  /** The on/off control at the right of a settings row. `labelledby` is the
   * id of the row's title, which names the switch for a screen reader.
   * `onchange` saves the new value; when the save is refused, the switch
   * goes back to where it was and says so, rather than showing a value that
   * was never stored. */
  let {
    checked,
    labelledby,
    disabled = false,
    onchange,
  }: {
    checked: boolean;
    labelledby: string;
    disabled?: boolean;
    onchange: (on: boolean) => Promise<unknown>;
  } = $props();

  let failed = $state(false);

  async function change(e: Event & { currentTarget: HTMLInputElement }) {
    const input = e.currentTarget;
    const on = input.checked;
    failed = false;
    try {
      await onchange(on);
    } catch (err) {
      console.error("settings switch not saved:", err);
      input.checked = !on;
      failed = true;
    }
  }
</script>

<div class="switch-wrap">
  <label class="switch" class:disabled>
    <input type="checkbox" {checked} {disabled} aria-labelledby={labelledby} onchange={change} />
    <span class="knob" aria-hidden="true"></span>
  </label>
  {#if failed}
    <span class="failed" role="alert">Couldn't save</span>
  {/if}
</div>

<style>
  .switch-wrap {
    flex: none;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 4px;
  }

  .switch {
    position: relative;
    display: block;
    width: 44px;
    height: 26px;
    cursor: pointer;
  }

  .switch.disabled {
    opacity: 0.5;
    pointer-events: none;
  }

  .switch input {
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }

  .knob {
    position: absolute;
    inset: 0;
    border-radius: 999px;
    background: var(--switch-off);
    transition: background 150ms ease;
  }

  .knob::after {
    content: "";
    position: absolute;
    top: 3px;
    left: 3px;
    width: 20px;
    height: 20px;
    border-radius: 50%;
    background: var(--switch-knob);
    transition: transform 150ms ease;
  }

  .switch input:checked + .knob {
    background: var(--accent);
  }

  .switch input:checked + .knob::after {
    transform: translateX(18px);
  }

  .switch input:focus-visible + .knob {
    box-shadow: var(--focus-ring);
  }

  .failed {
    font-size: 12px;
    color: var(--danger);
    white-space: nowrap;
  }
</style>
