<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";
  import { sarvamKeyStatus, type Provider } from "$lib/api";
  import { NAVIGATE, SETTINGS_CHANGED, type NavigatePayload } from "$lib/events";
  import { openRequest } from "$lib/openRequest.svelte";
  import { stats } from "$lib/stats.svelte";
  import { settings, ui } from "$lib/stores.svelte";
  import { update } from "$lib/update.svelte";
  import CommandPalette, { type PaletteItem } from "$lib/components/CommandPalette.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import TitleBar from "$lib/components/TitleBar.svelte";
  import Onboarding from "$lib/Onboarding.svelte";
  import Home from "$lib/pages/Home.svelte";
  import History from "$lib/pages/History.svelte";
  import Insights from "$lib/pages/Insights.svelte";
  import Dictionary from "$lib/pages/Dictionary.svelte";
  import Snippets from "$lib/pages/Snippets.svelte";
  import Style from "$lib/pages/Style.svelte";
  import Transforms from "$lib/pages/Transforms.svelte";
  import Notes from "$lib/pages/Notes.svelte";
  import Import from "$lib/pages/Import.svelte";
  import About from "$lib/pages/About.svelte";
  import SettingsModal, { type SectionId } from "$lib/settings/SettingsModal.svelte";

  const NAV_MAIN = [
    { id: "home", label: "Dictation", icon: "mic", component: Home },
    { id: "history", label: "History", icon: "clock", component: History },
    { id: "insights", label: "Insights", icon: "bars", component: Insights },
    { id: "dictionary", label: "Dictionary", icon: "book", component: Dictionary },
    { id: "snippets", label: "Snippets", icon: "scissors", component: Snippets },
    { id: "style", label: "Style", icon: "type", component: Style },
    { id: "transforms", label: "Transforms", icon: "sparkles", component: Transforms },
    // Notes moves the old Scratchpad's localStorage notes into SQLite on its
    // first load. The id is what `NavigatePayload.page` has to say —
    // `controller.rs`'s SHORTCUT_SCRATCHPAD emits it, and the NAVIGATE
    // handler below silently ignores a page it can't find, so a rename here
    // is a dead shortcut, not an error.
    { id: "notes", label: "Notes", icon: "note", component: Notes },
    { id: "import", label: "Import", icon: "upload", component: Import },
  ] as const;

  const NAV_BOTTOM = [
    { id: "about", label: "Help & about", icon: "help", component: About },
  ] as const;

  const ALL = [...NAV_MAIN, ...NAV_BOTTOM];

  let active = $state<(typeof ALL)[number]["id"]>("home");
  let keyMissing = $state(false);
  let settingsOpen = $state(false);
  let settingsSection = $state<SectionId>("general");
  let paletteOpen = $state(false);

  function openSettings(section: SectionId = "general") {
    settingsSection = section;
    settingsOpen = true;
  }

  function closeSettings() {
    settingsOpen = false;
    refreshKey();
  }

  let ActiveComponent = $derived(ALL.find((p) => p.id === active)?.component ?? Home);
  let provider = $derived<Provider>(settings.current?.provider ?? "sarvam");
  let onboarded = $derived(settings.current?.app.onboardingDone ?? false);

  /** The sidebar chip, one short name per engine. Cloud and On-device are
   * the Speech engine picker's own names; its "Bring your own key" is too
   * long for the chip, so the chip says "Your key". */
  const ENGINE_CHIP: Record<Provider, string> = {
    cloud: "Cloud",
    sarvam: "Your key",
    local: "On-device",
  };

  // A key is often saved while neither of the other refreshes can run:
  // onboarding validates one and switches to Bring your own key, then
  // finishes. Ask again whenever the engine or the onboarding state changes.
  $effect(() => {
    void provider;
    void onboarded;
    refreshKey();
  });

  async function refreshKey() {
    try {
      keyMissing = !(await sarvamKeyStatus()).present;
    } catch {
      keyMissing = false;
    }
  }

  function go(id: (typeof ALL)[number]["id"]) {
    active = id;
    refreshKey();
  }

  // Ctrl+K, the command palette. The typed letter decides first, so the
  // shortcut follows the K on any Latin layout, Dvorak included, and Caps
  // Lock makes no difference. When the active layout puts no Latin letter on
  // that key (InScript and the other Indic layouts), the physical K key
  // stands in, so those users keep the shortcut. Shift, Alt and the Windows
  // key must be up: Windows reports AltGr as Ctrl+Alt, and AltGr+K types a
  // character on several European layouts; Win+K is the system Cast panel.
  function isPaletteChord(e: KeyboardEvent): boolean {
    if (!e.ctrlKey || e.altKey || e.shiftKey || e.metaKey) return false;
    const latin = e.key.length === 1 && /^[a-z]$/i.test(e.key);
    return latin ? e.key.toLowerCase() === "k" : e.code === "KeyK";
  }

  // Auto-repeat lands here too; it only sets an open flag that is already set.
  function onKeydown(e: KeyboardEvent) {
    if (!isPaletteChord(e)) return;
    // The shell is not up during onboarding or a failed settings load — there
    // is nothing to jump to, and the palette would cover the only thing on
    // screen.
    if (!settings.current?.app.onboardingDone) return;
    // Nothing may open over an overlay that already owns the keyboard.
    //
    // `ui.capturingShortcut` is the house rule `stores.svelte.ts` states:
    // while a shortcut recorder (the Shortcuts dialog, or the Transforms
    // editor's) is listening, the rdev hook observes the chord without
    // consuming it, so this window sees the very keydown the user is trying
    // to *bind* — Ctrl+K would record a chord and open the palette on top of
    // the recorder in the same press.
    //
    // `settingsOpen` is the same argument one level out. The palette's scrim
    // sits above the modal's, and picking a result navigates the page
    // *behind* Settings (`go()` does not close it), so the note would open
    // where nobody can see it.
    if (settingsOpen || ui.capturingShortcut) return;
    e.preventDefault();
    paletteOpen = true;
  }

  /**
   * Route a picked result to the page that shows it.
   *
   * The order matters: the request is parked *before* `go()`, because the
   * destination page is usually mounted by that navigation and reads the slot
   * on its first effect.
   */
  function openFromPalette(item: PaletteItem) {
    paletteOpen = false;
    if (item.kind === "dictation") {
      openRequest.set({ kind: "dictation", entry: item.entry });
      go("history");
      return;
    }
    if (item.kind === "note") {
      openRequest.set({ kind: "note", note: item.note });
    } else {
      openRequest.set({ kind: "folder", folderId: item.folder.id });
    }
    go("notes");
  }

  onMount(() => {
    settings.loadWithRetry();
    stats.init();
    update.init();
    let unsub: (() => void) | undefined;
    let unsubSettings: (() => void) | undefined;
    listen<NavigatePayload>(NAVIGATE, (e) => {
      const target = ALL.find((p) => p.id === e.payload.page);
      if (target) {
        settingsOpen = false;
        go(target.id);
      }
    }).then((u) => (unsub = u));
    // The backend wrote settings without being asked (auto-learn promoted a
    // correction). This window loads settings once per life and every write
    // it makes is a whole-object save, so without this reload the next toggle
    // the user flips would persist the pre-promotion copy and silently delete
    // the learned rule. Listened for at the shell rather than on the
    // Dictionary page: the wipe happens wherever the *next* write happens,
    // which is any settings control in the app.
    listen(SETTINGS_CHANGED, () => {
      settings.reload();
    }).then((u) => (unsubSettings = u));
    return () => {
      unsub?.();
      unsubSettings?.();
    };
  });
</script>

<svelte:window onkeydown={onKeydown} />

{#if !settings.current}
  <div class="frame">
    <TitleBar />
    <div class="boot">
      {#if settings.error}
        <p class="boot-title">Butterfly Speak couldn't load its settings</p>
        <p class="boot-msg">{settings.error}</p>
        <button class="boot-retry" onclick={() => settings.loadWithRetry()}>Try again</button>
      {:else}
        <p class="boot-msg">Starting…</p>
      {/if}
    </div>
  </div>
{:else if !settings.current.app.onboardingDone}
  <div class="frame">
    <TitleBar />
    <div class="scroll">
      <Onboarding />
    </div>
  </div>
{:else}
  <div class="frame">
    <TitleBar />
    <div class="body">
      <nav>
        <div class="brand">
          <!-- The suite lockup: the shared silhouette at text height in
               currentColor, the product's lower-right lobe in saffron with the
               listening-waveform glyph cut in the sidebar colour, then
               "Butterfly" regular and the product name bold. -->
          <svg class="brand-mark" viewBox="0 0 24 24" aria-hidden="true">
            <g fill="currentColor">
              <circle cx="7.2" cy="9" r="4.8" />
              <circle cx="8.2" cy="16" r="3.6" />
              <circle cx="16.8" cy="9" r="4.8" />
              <rect x="11.15" y="7.4" width="1.7" height="10" rx="0.85" />
            </g>
            <g fill="none" stroke="currentColor" stroke-width="1" stroke-linecap="round">
              <path d="M11.7 7.6 C11.1 5.8 10 4.6 8.8 3.9" />
              <path d="M12.3 7.6 C12.9 5.8 14 4.6 15.2 3.9" />
            </g>
            <circle class="brand-lobe" cx="15.8" cy="16" r="3.6" />
            <g class="brand-glyph" transform="translate(15.8 16)">
              <rect x="-1.6" y="-0.9" width="0.78" height="1.8" rx="0.39" />
              <rect x="-0.39" y="-1.75" width="0.78" height="3.5" rx="0.39" />
              <rect x="0.82" y="-1.2" width="0.78" height="2.4" rx="0.39" />
            </g>
          </svg>
          <span class="brand-name">Butterfly <strong>Speak</strong></span>
          <span class="brand-chip">{ENGINE_CHIP[provider]}</span>
        </div>

        <!-- Without this the palette is a keystroke nobody is told about. -->
        <button class="palette-btn" onclick={() => (paletteOpen = true)}>
          <Icon name="search" size={16} stroke={1.8} />
          <span>Search</span>
          <kbd>Ctrl K</kbd>
        </button>

        <div class="nav-group">
          {#each NAV_MAIN as page}
            <button
              class="nav-item"
              class:active={active === page.id}
              onclick={() => go(page.id)}
            >
              <Icon name={page.icon} size={18} />
              <span>{page.label}</span>
            </button>
          {/each}
        </div>

        <div class="spacer"></div>

        {#if stats.totalWords > 0}
          <div class="side-stats">
            <div class="ss-row">
              <span class="ss-num">{stats.todayWords.toLocaleString()}</span>
              <span class="ss-label">words today</span>
            </div>
            <div class="ss-row">
              <span class="ss-num">{stats.streak}</span>
              <span class="ss-label">day streak</span>
            </div>
          </div>
        {/if}

        {#if provider === "sarvam" && keyMissing}
          <div class="connect-card">
            <p class="connect-title">Connect Sarvam AI</p>
            <p class="connect-body">
              Dictation needs your API key. New accounts include free credits.
            </p>
            <button class="connect-btn" onclick={() => openSettings("engine")}>Connect</button>
          </div>
        {/if}

        {#if update.card}
          {@const card = update.card}
          <div class="connect-card update-card">
            {#if card.kind === "error"}
              <p class="connect-title">Update didn't install</p>
              <p class="connect-body">{card.message}</p>
              <div class="update-actions">
                <button class="connect-btn" onclick={() => update.check("card")}>Try again</button>
                <button class="update-later" onclick={() => (update.lastSurface = null)}>
                  Dismiss
                </button>
              </div>
            {:else}
              <p class="connect-title">Butterfly Speak {card.version} is ready</p>
              <p class="connect-body">Installs in about a minute and restarts the app.</p>
              <div class="update-actions">
                <button
                  class="connect-btn"
                  disabled={card.kind !== "available"}
                  onclick={() => update.install("card")}
                >
                  {#if card.kind === "downloading"}
                    Downloading{card.percent === null ? "…" : ` ${card.percent}%`}
                  {:else if card.kind === "installing"}
                    Restarting…
                  {:else}
                    Install and restart
                  {/if}
                </button>
                {#if card.kind === "available"}
                  <button class="update-later" onclick={() => update.dismiss()}>Later</button>
                {/if}
              </div>
            {/if}
          </div>
        {/if}

        <div class="nav-group bottom">
          <button class="nav-item" onclick={() => openSettings()}>
            <Icon name="gear" size={18} />
            <span>Settings</span>
          </button>
          {#each NAV_BOTTOM as page}
            <button
              class="nav-item"
              class:active={active === page.id}
              onclick={() => go(page.id)}
            >
              <Icon name={page.icon} size={18} />
              <span>{page.label}</span>
            </button>
          {/each}
        </div>
      </nav>

      <main>
        <div class="panel">
          <ActiveComponent />
        </div>
      </main>
    </div>

    {#if settingsOpen}
      <SettingsModal section={settingsSection} onclose={closeSettings} />
    {/if}

    {#if paletteOpen}
      <CommandPalette onclose={() => (paletteOpen = false)} onselect={openFromPalette} />
    {/if}
  </div>
{/if}

<style>
  .frame {
    height: 100vh;
    display: flex;
    flex-direction: column;
    background: var(--bg);
    overflow: hidden;
  }

  .scroll {
    flex: 1;
    overflow-y: auto;
  }

  /* Shown while settings are loading, and if that load ultimately fails —
     without it a failed round-trip leaves nothing but the title bar. */
  .boot {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 10px;
    padding: 24px;
    text-align: center;
  }

  .boot-title {
    font-size: 15px;
    font-weight: 600;
    color: var(--fg);
  }

  .boot-msg {
    font-size: 13px;
    color: var(--fg-muted);
    max-width: 460px;
    word-break: break-word;
  }

  .boot-retry {
    margin-top: 4px;
    padding: 8px 18px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    background: var(--surface);
    font-size: 13px;
    color: var(--fg);
    cursor: pointer;
  }

  .boot-retry:hover {
    background: var(--bg-elevated);
  }

  .body {
    flex: 1;
    display: flex;
    min-height: 0;
  }

  nav {
    flex: none;
    width: var(--sidebar-w);
    padding: 0 14px 16px;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 10px 14px;
    margin-bottom: 8px;
    border-bottom: 1px solid var(--hairline);
  }

  .brand-mark {
    width: 22px;
    height: 22px;
    color: var(--fg);
    flex: none;
  }

  /* The one accent in the app chrome — the lockup is one of the three places
     the product colour may appear (the app icon, the lockup, the listening
     waveform); the glyph is cut in the sidebar colour so the lobe reads as a
     hole, not a badge. */
  .brand-lobe {
    fill: var(--brand-saffron);
  }

  .brand-glyph {
    fill: var(--bg);
  }

  .brand-name {
    font-weight: 450;
    font-size: 15px;
    letter-spacing: -0.01em;
  }

  .brand-name strong {
    font-weight: 700;
  }

  .brand-chip {
    font-size: 11px;
    font-weight: 600;
    border: 1px solid var(--hairline-strong);
    border-radius: 7px;
    padding: 2px 8px;
    color: var(--fg-muted);
    background: var(--surface);
  }

  .palette-btn {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 500;
    text-align: left;
    padding: 8px 12px;
    margin-bottom: 8px;
    border: 1px solid var(--hairline-strong);
    border-radius: 10px;
    background: var(--surface);
    color: var(--fg-muted);
    cursor: pointer;
    transition: background var(--motion), border-color var(--motion);
  }

  .palette-btn:hover {
    background: var(--bg-elevated);
    border-color: var(--hairline-hover);
  }

  .palette-btn :global(svg) {
    color: var(--fg-faint);
    flex: none;
  }

  .palette-btn span {
    flex: 1;
  }

  .palette-btn kbd {
    font-family: var(--font-ui);
    font-size: 10.5px;
    font-weight: 600;
    background: var(--chip);
    border-radius: 5px;
    padding: 2px 6px;
    color: var(--fg-faint);
  }

  .nav-group {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .nav-item {
    display: flex;
    align-items: center;
    gap: 12px;
    font-family: var(--font-ui);
    font-size: 14.5px;
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

  .nav-item :global(svg) {
    color: var(--fg-muted);
    flex: none;
  }

  .nav-item:hover {
    background: var(--wash);
  }

  .nav-item.active {
    background: var(--selected);
    font-weight: 600;
  }

  .nav-item.active :global(svg) {
    color: var(--fg);
  }

  .spacer {
    flex: 1;
  }

  .side-stats {
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    background: var(--surface);
    padding: 12px 16px;
    margin: 0 2px 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .ss-row {
    display: flex;
    align-items: baseline;
    gap: 8px;
  }

  .ss-num {
    font-family: var(--font-display);
    font-size: 19px;
    line-height: 1;
  }

  .ss-label {
    font-size: 12px;
    color: var(--fg-muted);
  }

  .connect-card {
    background: var(--promo-bg);
    border-radius: var(--radius-card);
    padding: 14px 14px 16px;
    margin: 0 2px 14px;
  }

  .connect-title {
    font-size: 14px;
    font-weight: 650;
    margin: 0 0 4px;
  }

  .connect-body {
    font-size: 12.5px;
    line-height: 1.5;
    color: var(--fg-muted);
    margin: 0 0 12px;
  }

  .connect-btn {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    border: none;
    border-radius: var(--radius-control);
    background: var(--accent);
    color: var(--accent-fg);
    padding: 8px 16px;
    cursor: pointer;
  }

  .update-actions {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .connect-btn:disabled {
    opacity: 0.7;
    cursor: default;
  }

  .update-later {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 500;
    border: none;
    background: transparent;
    color: var(--fg-muted);
    padding: 8px 4px;
    cursor: pointer;
  }

  .update-later:hover {
    color: var(--fg);
  }

  .nav-group.bottom {
    padding-top: 10px;
  }

  main {
    flex: 1;
    min-width: 0;
    padding: 0 14px 14px 6px;
    display: flex;
  }

  .panel {
    flex: 1;
    min-width: 0;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-panel);
    box-shadow: var(--shadow-panel);
    overflow-y: auto;
    padding: 44px 48px 40px;
  }
</style>
