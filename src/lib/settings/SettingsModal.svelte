<script lang="ts">
  import { onMount } from "svelte";
  import { systemInfo } from "$lib/api";
  import { ui } from "$lib/stores.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import GeneralSection from "./GeneralSection.svelte";
  import AppearanceSection from "./AppearanceSection.svelte";
  import SystemSection from "./SystemSection.svelte";
  import EngineSection from "./EngineSection.svelte";
  import CleanupSection from "./CleanupSection.svelte";
  import PromptsSection from "./PromptsSection.svelte";
  import EndpointSection from "./EndpointSection.svelte";

  export type SectionId =
    | "general"
    | "appearance"
    | "system"
    | "engine"
    | "cleanup"
    | "prompts"
    | "endpoint";

  let {
    section = "general",
    onclose,
  }: { section?: SectionId; onclose: () => void } = $props();

  const SECTIONS = [
    { id: "general", label: "General", icon: "sliders", component: GeneralSection },
    { id: "appearance", label: "Appearance", icon: "sun", component: AppearanceSection },
    { id: "system", label: "System", icon: "monitor", component: SystemSection },
    { id: "engine", label: "Speech engine", icon: "globe", component: EngineSection },
    { id: "cleanup", label: "Cleanup", icon: "sparkles", component: CleanupSection },
    { id: "prompts", label: "Prompts", icon: "type", component: PromptsSection },
    // Last on the rail on purpose: it is the only section that can point the
    // app at a host Butterfly Speak has never spoken to, and nobody should
    // land on it while looking for something else.
    { id: "endpoint", label: "Custom endpoint", icon: "server", component: EndpointSection },
  ] as const;

  // svelte-ignore state_referenced_locally -- the prop only seeds which
  // section opens first; navigation afterwards is local.
  let active = $state<SectionId>(section);
  let version = $state("");

  let Active = $derived(SECTIONS.find((s) => s.id === active)?.component ?? GeneralSection);

  onMount(() => {
    systemInfo().then((i) => (version = i.appVersion));
  });
</script>

<svelte:window
  onkeydown={(e) => {
    // A layer inside the modal (a Dropdown's panel, the Shortcuts dialog)
    // takes its Escape at the window's capture phase and stops it before it
    // gets here. A shortcut recorder outside the modal (the Transforms
    // editor, still mounted behind it) does not, so while one is recording,
    // Escape cancels the recording and leaves the modal open.
    if (e.key === "Escape" && !ui.capturingShortcut) onclose();
  }}
/>

<div
  class="scrim"
  role="presentation"
  onclick={(e) => {
    if (e.target === e.currentTarget) onclose();
  }}
>
  <div class="modal" role="dialog" aria-modal="true" aria-label="Settings">
    <aside class="rail">
      <p class="rail-label">Settings</p>
      <div class="rail-items">
        {#each SECTIONS as s (s.id)}
          <button
            class="rail-item"
            class:active={active === s.id}
            onclick={() => (active = s.id)}
          >
            <Icon name={s.icon} size={17} stroke={1.7} />
            <span>{s.label}</span>
          </button>
        {/each}
      </div>
      <div class="rail-spacer"></div>
      <div class="rail-version">
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <g fill="currentColor">
            <circle cx="7.2" cy="9" r="4.8" />
            <circle cx="8.2" cy="16" r="3.6" />
            <circle cx="16.8" cy="9" r="4.8" />
            <circle cx="15.8" cy="16" r="3.6" />
          </g>
        </svg>
        <span>Butterfly Speak {version ? `v${version}` : ""}</span>
      </div>
    </aside>
    <div class="content">
      <Active />
    </div>
  </div>
</div>

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 50;
    background: var(--scrim);
    display: grid;
    place-items: center;
    animation: fade 140ms ease;
  }

  @keyframes fade {
    from {
      opacity: 0;
    }
  }

  .modal {
    width: min(1060px, calc(100vw - 120px));
    height: min(780px, calc(100vh - 96px));
    background: var(--surface);
    border-radius: var(--radius-panel);
    box-shadow: var(--shadow-modal);
    display: flex;
    overflow: hidden;
    animation: rise 160ms ease;
  }

  @keyframes rise {
    from {
      opacity: 0;
      transform: translateY(8px) scale(0.99);
    }
  }

  .rail {
    flex: none;
    width: 248px;
    border-right: 1px solid var(--hairline);
    background: var(--rail-bg);
    padding: 26px 16px 20px;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .rail-label {
    font-size: 11px;
    font-weight: 650;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--fg-muted);
    margin: 0 0 14px;
    padding: 0 12px;
  }

  .rail-items {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .rail-item {
    display: flex;
    align-items: center;
    gap: 12px;
    font-family: var(--font-ui);
    font-size: 14px;
    font-weight: 500;
    text-align: left;
    padding: 10px 12px;
    border: none;
    border-radius: 10px;
    background: transparent;
    color: var(--fg);
    cursor: pointer;
    transition: background var(--motion);
  }

  .rail-item :global(svg) {
    color: var(--fg-muted);
    flex: none;
  }

  .rail-item:hover {
    background: var(--wash);
  }

  .rail-item.active {
    background: var(--chip);
    font-weight: 600;
  }

  .rail-item.active :global(svg) {
    color: var(--fg);
  }

  .rail-spacer {
    flex: 1;
  }

  .rail-version {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 0 12px;
    font-size: 12.5px;
    color: var(--fg-muted);
  }

  .rail-version svg {
    width: 15px;
    height: 15px;
    color: var(--fg-faint);
    flex: none;
  }

  .content {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 44px 52px 44px;
  }
</style>
