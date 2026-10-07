<script lang="ts">
  import Icon from "./Icon.svelte";

  // A select: trigger with chevron; floating panel with optional search,
  // two-line options, and a checkmark on the current value. The panel
  // is position:fixed so it escapes scrolling/overflow ancestors (modals).
  export interface DropdownOption {
    value: string;
    label: string;
    sublabel?: string;
  }

  let {
    options,
    value,
    onchange,
    searchable = undefined,
    placeholder = "Select…",
    ariaLabel = undefined,
    compact = false,
  }: {
    options: DropdownOption[];
    value: string;
    onchange: (v: string) => void | Promise<void>;
    /** Defaults to true when there are more than 8 options. */
    searchable?: boolean;
    placeholder?: string;
    /** Accessible name for the trigger button. Optional — skip it where
     * adjacent visible text already labels the control; pass it where the
     * control has no associated text (a table/list row, a bare form). */
    ariaLabel?: string;
    /** Toolbar sizing: the settings rows this was built for give a control a
     * whole row, a toolbar gives it the height of a button. Only the trigger
     * changes; the panel is the same either way, so the two sizes stay one
     * component instead of becoming two. */
    compact?: boolean;
  } = $props();

  let root: HTMLDivElement | undefined = $state();
  let trigger: HTMLButtonElement | undefined = $state();
  let searchEl: HTMLInputElement | undefined = $state();
  let open = $state(false);
  let query = $state("");
  let panelStyle = $state("");

  let current = $derived(options.find((o) => o.value === value));
  let showSearch = $derived(searchable ?? options.length > 8);
  let filtered = $derived(
    query.trim()
      ? options.filter((o) =>
          `${o.label} ${o.sublabel ?? ""}`.toLowerCase().includes(query.trim().toLowerCase()),
        )
      : options,
  );

  const PANEL_MAX = 340;

  function toggle() {
    if (open) {
      close();
      return;
    }
    if (!trigger) return;
    const r = trigger.getBoundingClientRect();
    const width = Math.max(r.width, 280);
    const left = Math.min(r.left, window.innerWidth - width - 12);
    const below = window.innerHeight - r.bottom;
    if (below < PANEL_MAX && r.top > below) {
      panelStyle = `left:${left}px; bottom:${window.innerHeight - r.top + 6}px; width:${width}px;`;
    } else {
      panelStyle = `left:${left}px; top:${r.bottom + 6}px; width:${width}px;`;
    }
    query = "";
    open = true;
    // Focus the search box once the panel exists.
    requestAnimationFrame(() => searchEl?.focus());
  }

  function close() {
    open = false;
    query = "";
  }

  async function pick(v: string) {
    close();
    if (v !== value) await onchange(v);
  }

  function onDocPointerDown(e: PointerEvent) {
    if (open && root && !root.contains(e.target as Node)) close();
  }

  function onDocScroll(e: Event) {
    // Scrolling any ancestor desyncs the fixed panel — just close.
    if (open && root && !root.contains(e.target as Node)) close();
  }
</script>

<!-- Keyboard support is Escape-to-close only: no Arrow/Home/End roving
     focus and no `aria-activedescendant` on the listbox (the `role="option"`
     buttons below are only mouse/pointer targets). Adding it is a design
     decision, not a patch: a searchable panel (`GeneralSection`'s language
     picker, 24 options) already puts focus in a search input that would
     compete with arrow keys, and roving tabindex vs. activedescendant,
     wraparound, and typeahead vs. the search box all need clicking through
     in the running app. -->
<svelte:document onpointerdowncapture={onDocPointerDown} onscrollcapture={onDocScroll} />
<!-- Captured at the window, as the command palette does, so an open panel
     takes Escape before the dialog it sits in. Svelte adds a parent's window
     listener before its children's, so a bubbling handler here would run
     after the Settings modal had already closed on the same key. -->
<svelte:window
  onkeydowncapture={(e) => {
    if (open && e.key === "Escape") {
      e.stopPropagation();
      close();
    }
  }}
/>

<div class="dd" bind:this={root}>
  <button
    class="trigger"
    class:compact
    type="button"
    bind:this={trigger}
    aria-haspopup="listbox"
    aria-expanded={open}
    aria-label={ariaLabel}
    onclick={toggle}
  >
    <span class="trigger-label" class:faint={!current}>{current?.label ?? placeholder}</span>
    <span class="chev" class:open><Icon name="chevron-down" size={15} stroke={1.8} /></span>
  </button>

  {#if open}
    <div class="panel" style={panelStyle} role="listbox">
      {#if showSearch}
        <div class="search">
          <Icon name="search" size={15} stroke={1.7} />
          <input
            bind:this={searchEl}
            bind:value={query}
            placeholder="Search"
            onkeydown={(e) => {
              if (e.key === "Enter" && filtered.length > 0) pick(filtered[0].value);
            }}
          />
        </div>
      {/if}
      <div class="list">
        {#each filtered as o (o.value)}
          <button
            class="option"
            type="button"
            role="option"
            aria-selected={o.value === value}
            onclick={() => pick(o.value)}
          >
            <span class="texts">
              <span class="label">{o.label}</span>
              {#if o.sublabel}
                <span class="sublabel">{o.sublabel}</span>
              {/if}
            </span>
            {#if o.value === value}
              <Icon name="check" size={15} stroke={2} />
            {/if}
          </button>
        {:else}
          <p class="none">No matches</p>
        {/each}
      </div>
    </div>
  {/if}
</div>

<style>
  .dd {
    position: relative;
    flex: none;
  }

  .trigger.compact {
    min-width: 0;
    max-width: 200px;
    font-size: 12.5px;
    padding: 4px 8px 4px 10px;
    border-radius: var(--radius-control);
    border-color: var(--hairline);
    gap: 6px;
  }

  .trigger.compact:hover {
    border-color: var(--hairline-strong);
  }

  .trigger {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 170px;
    max-width: 280px;
    font-family: var(--font-ui);
    font-size: 13.5px;
    font-weight: 500;
    padding: 9px 12px 9px 14px;
    border: 1px solid var(--hairline-strong);
    border-radius: 10px;
    background: var(--surface);
    color: var(--fg);
    cursor: pointer;
    transition: border-color var(--motion);
  }

  .trigger:hover {
    border-color: var(--hairline-hover);
  }

  .trigger-label {
    flex: 1;
    min-width: 0;
    text-align: left;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .trigger-label.faint {
    color: var(--fg-faint);
  }

  .chev {
    flex: none;
    display: grid;
    place-items: center;
    color: var(--fg-muted);
    transition: transform var(--motion);
  }

  .chev.open {
    transform: rotate(180deg);
  }

  .panel {
    position: fixed;
    z-index: 90;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: 12px;
    box-shadow: var(--shadow-pop);
    padding: 8px;
    animation: pop 120ms ease;
  }

  @keyframes pop {
    from {
      opacity: 0;
      transform: translateY(-4px);
    }
  }

  .search {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    margin-bottom: 6px;
    border-bottom: 1px solid var(--hairline);
    color: var(--fg-faint);
  }

  .search input {
    flex: 1;
    min-width: 0;
    border: none;
    outline: none;
    background: transparent;
    font-family: var(--font-ui);
    font-size: 13.5px;
    color: var(--fg);
    padding: 4px 0;
  }

  .search input::placeholder {
    color: var(--fg-faint);
  }

  .list {
    max-height: 300px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }

  .option {
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    text-align: left;
    border: none;
    background: transparent;
    border-radius: 8px;
    padding: 9px 10px;
    cursor: pointer;
    color: var(--fg);
    font-family: var(--font-ui);
  }

  .option:hover {
    background: var(--wash);
  }

  .option :global(svg) {
    flex: none;
    color: var(--fg);
  }

  .texts {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .label {
    font-size: 14px;
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .sublabel {
    font-size: 12.5px;
    color: var(--fg-muted);
    /* Panel width is fixed (~280px) regardless of content, so a longer
       sublabel needs room to wrap rather than lose the clause that
       distinguishes it from the option next to it. Clamped to two lines —
       still bounded, just not to a single truncated line. */
    display: -webkit-box;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
    line-clamp: 2;
    white-space: normal;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .none {
    font-size: 13px;
    color: var(--fg-faint);
    text-align: center;
    padding: 14px 0;
    margin: 0;
  }
</style>
