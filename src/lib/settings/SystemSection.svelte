<script lang="ts">
  import "./rows.css";
  import {
    exportSettings,
    importSettings,
    pickNotesMirrorDir,
    rebuildNotesMirror,
  } from "$lib/api";
  import { settings } from "$lib/stores.svelte";
  import Dropdown, { type DropdownOption } from "$lib/components/Dropdown.svelte";
  import Switch from "./Switch.svelte";

  let s = $derived(settings.current);

  const KEEP_DAYS_OPTIONS: DropdownOption[] = [
    { value: "0", label: "Forever" },
    { value: "1", label: "1 day" },
    { value: "7", label: "7 days" },
    { value: "14", label: "14 days" },
    { value: "30", label: "30 days" },
    { value: "60", label: "60 days" },
    { value: "90", label: "90 days" },
  ];

  let exportState = $state<"idle" | "busy" | "done">("idle");
  let importState = $state<"idle" | "busy" | "done">("idle");
  let importError = $state("");

  async function doExport() {
    exportState = "busy";
    try {
      const saved = await exportSettings();
      if (saved) {
        exportState = "done";
        setTimeout(() => (exportState = "idle"), 1600);
      } else {
        exportState = "idle";
      }
    } catch (e) {
      console.error("export settings failed:", e);
      exportState = "idle";
    }
  }

  let mirrorError = $state("");
  let rebuildState = $state<"idle" | "busy">("idle");
  let rebuildResult = $state("");

  /** The mirror only writes when it is both on AND pointed somewhere — the
   * same rule `NotesSettings::mirror_root` enforces on the Rust side. The
   * toggle is disabled without a folder so the UI cannot show "on" for a
   * state that writes nothing. */
  let mirrorReady = $derived(!!s?.notes.mirrorDir);

  async function chooseMirrorDir() {
    mirrorError = "";
    rebuildResult = "";
    try {
      const dir = await pickNotesMirrorDir();
      if (!dir) return;
      // Choosing a folder is what turns the mirror on: picking one and then
      // finding nothing happened would be confusing. Awaited so a failed
      // save lands in `mirrorError` below instead of going unseen.
      await settings.update((st) => {
        st.notes.mirrorDir = dir;
        st.notes.mirrorEnabled = true;
      });
    } catch (e) {
      mirrorError = String(e);
    }
  }

  async function doRebuild() {
    rebuildState = "busy";
    rebuildResult = "";
    mirrorError = "";
    try {
      const written = await rebuildNotesMirror();
      rebuildResult = `Wrote ${written} note${written === 1 ? "" : "s"}.`;
    } catch (e) {
      mirrorError = String(e);
    } finally {
      rebuildState = "idle";
    }
  }

  /** The most the pill may sit above the bottom of the screen, in pixels.
   * The field's arrows stop at 0 and at this, but typing can go past both. */
  const PILL_OFFSET_MAX = 400;
  let pillError = $state("");

  /** Clamp what was typed into range and save it. An empty or unreadable
   * field goes back to the stored value rather than to a default, and 0 is
   * a real position. */
  async function setPillOffset(input: HTMLInputElement) {
    pillError = "";
    const stored = s?.overlay.offsetY ?? 0;
    const typed = input.value.trim() === "" ? NaN : Number(input.value);
    const next = Number.isFinite(typed)
      ? Math.min(PILL_OFFSET_MAX, Math.max(0, Math.round(typed)))
      : stored;
    input.value = String(next);
    if (next === stored) return;
    try {
      await settings.update((st) => (st.overlay.offsetY = next));
    } catch (e) {
      console.error("pill position not saved:", e);
      input.value = String(stored);
      pillError = "Couldn't save the pill position. Try again in a moment.";
    }
  }

  async function doImport() {
    importState = "busy";
    importError = "";
    try {
      const applied = await importSettings();
      if (applied) {
        // import_settings persists straight to disk + Backend without going
        // through this window's setSettings() call, so `settings.current`
        // is still the pre-import snapshot until we reload it here. Without
        // this reload the next settings.update() would clone the stale
        // snapshot and write it back over the import. Queued, not `load`:
        // it waits behind any save still in flight, which would otherwise
        // land after it and bring the old snapshot back. A failed re-read
        // lands in `importError` below.
        await settings.reloadChecked();
        importState = "done";
        setTimeout(() => (importState = "idle"), 1600);
      } else {
        importState = "idle";
      }
    } catch (e) {
      importError = String(e);
      importState = "idle";
    }
  }
</script>

{#if s}
  <h1 class="s-title">System</h1>

  <p class="s-group">App settings</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-launch-at-login">Launch app at login</p>
        <p class="s-row-sub">Start quietly in the tray when you sign in.</p>
      </div>
      <Switch
        labelledby="set-launch-at-login"
        checked={s.app.launchAtLogin}
        onchange={(on) => settings.update((st) => (st.app.launchAtLogin = on))}
      />
    </div>

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-auto-update">Check for updates automatically</p>
        <p class="s-row-sub">
          Looks for a new version half a minute after Butterfly Speak starts
          and every eight hours. Nothing is downloaded or installed until you
          choose to — you can always check by hand under Help &amp; about.
        </p>
      </div>
      <Switch
        labelledby="set-auto-update"
        checked={s.updates.autoCheck}
        onchange={(on) => settings.update((st) => (st.updates.autoCheck = on))}
      />
    </div>
  </div>

  <p class="s-group">Learning from your corrections</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-learn">Learn from corrections you make</p>
        <p class="s-row-sub">
          After Butterfly Speak types a dictation into another app, it reads
          that text field for a short while to see whether you fixed a word by
          hand. This happens on your device and nothing is sent anywhere. A
          single fix changes nothing: the same correction has to come back in a
          second dictation before it becomes a rule, and you'll get a
          notification when it does — remove it any time under Dictionary →
          Corrections.
        </p>
      </div>
      <Switch
        labelledby="set-learn"
        checked={s.learn.fieldMonitorEnabled}
        onchange={(on) => settings.update((st) => (st.learn.fieldMonitorEnabled = on))}
      />
    </div>
  </div>

  <p class="s-group">History &amp; retention</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-history">Keep dictation history</p>
        <p class="s-row-sub">
          Saves your dictations on this device so you can search and revisit them
          in History and on Home. Turning this off doesn't delete what's already
          saved; while it's off, Home shows only this session's dictations and
          forgets them when the app closes. The word counts behind Insights are
          kept either way and hold no text. So are the single words you
          correct: fixing a word in an edit on Home adds a rule under
          Dictionary → Corrections,
          and while learning from corrections is on, each word you fix is kept
          for up to 30 days after you last fixed it, as evidence toward a rule.
          Neither setting here, nor Clear all history on the History page,
          removes those.
        </p>
      </div>
      <Switch
        labelledby="set-history"
        checked={s.history.enabled}
        onchange={(on) => settings.update((st) => (st.history.enabled = on))}
      />
    </div>

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Keep transcripts for</p>
        <p class="s-row-sub">
          Older dictations are removed automatically. This is local-only data —
          "Forever" is the default.
        </p>
      </div>
      <Dropdown
        options={KEEP_DAYS_OPTIONS}
        value={String(s.history.keepDays)}
        onchange={(v) => settings.update((st) => (st.history.keepDays = Number(v) || 0))}
      />
    </div>
  </div>

  <p class="s-group">Notes on disk</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-notes-mirror">Keep a Markdown copy of every note</p>
        <p class="s-row-sub">
          Writes one <code>.md</code> file per note into a folder you choose, so
          your notes are readable in any Markdown editor.
          The copy goes one way: Butterfly Speak writes the files and never
          reads them back, so editing one outside the app won't change the note
          here.
        </p>
        {#if s.notes.mirrorDir}
          <p class="s-path">{s.notes.mirrorDir}</p>
        {:else}
          <p class="s-row-sub">Choose a folder to turn this on.</p>
        {/if}
        {#if mirrorError}
          <p class="s-error">{mirrorError}</p>
        {/if}
        {#if rebuildResult}
          <p class="s-row-sub">{rebuildResult}</p>
        {/if}
      </div>
      <Switch
        labelledby="set-notes-mirror"
        checked={s.notes.mirrorEnabled && mirrorReady}
        disabled={!mirrorReady}
        onchange={(on) => settings.update((st) => (st.notes.mirrorEnabled = on))}
      />
    </div>

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Folder</p>
        <p class="s-row-sub">
          Where the files go. Each note lands at
          <code>&lt;folder&gt;/&lt;its folder&gt;/&lt;id&gt; &lt;title&gt;.md</code>,
          and notes in no folder go to <code>Unfiled</code>.
        </p>
      </div>
      <button class="s-btn" onclick={chooseMirrorDir}>
        {s.notes.mirrorDir ? "Change…" : "Choose…"}
      </button>
    </div>

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Write all notes now</p>
        <p class="s-row-sub">
          Turning the copy on only covers notes you edit from then on. This
          writes every note you already have.
        </p>
      </div>
      <button
        class="s-btn"
        onclick={doRebuild}
        disabled={rebuildState === "busy" || !s.notes.mirrorEnabled || !mirrorReady}
      >
        {rebuildState === "busy" ? "Writing…" : "Write all"}
      </button>
    </div>
  </div>

  <p class="s-group">Backup</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Export settings</p>
        <p class="s-row-sub">
          Saves your settings to a JSON file — dictionary, snippets, transforms,
          and preferences. Your Sarvam API key is never included; it stays in
          Windows' credential store.
        </p>
      </div>
      <button class="s-btn" onclick={doExport} disabled={exportState === "busy"}>
        {exportState === "done" ? "Saved" : exportState === "busy" ? "Saving…" : "Export…"}
      </button>
    </div>

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Import settings</p>
        <p class="s-row-sub">
          Load settings from a previously exported file. Replaces your current
          settings, except the notes folder, which stays as chosen on this
          computer.
        </p>
        {#if importError}
          <p class="s-error">{importError}</p>
        {/if}
      </div>
      <button class="s-btn" onclick={doImport} disabled={importState === "busy"}>
        {importState === "done" ? "Imported" : importState === "busy" ? "Importing…" : "Import…"}
      </button>
    </div>
  </div>

  <p class="s-group">Overlay</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-pill-position">Pill position</p>
        <p class="s-row-sub">
          Distance from the bottom of the screen, in pixels.
        </p>
      </div>
      <div class="pill-offset">
        <input
          class="s-input"
          type="number"
          min="0"
          max={PILL_OFFSET_MAX}
          style="max-width:110px"
          aria-labelledby="set-pill-position"
          value={s.overlay.offsetY}
          onchange={(e) => setPillOffset(e.currentTarget)}
        />
        {#if pillError}
          <p class="s-error">{pillError}</p>
        {/if}
      </div>
    </div>
  </div>

  <p class="s-group">Window</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Closing the window</p>
        <p class="s-row-sub">
          Closing keeps Butterfly Speak running in the system tray so dictation
          stays available. Quit from the tray icon.
        </p>
      </div>
    </div>
  </div>
{/if}

<style>
  /* The chosen mirror folder. Wraps rather than truncates: a path the user
     cannot read in full is a path they cannot check. */
  .s-path {
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 12.5px;
    color: var(--fg-muted);
    margin: 6px 0 0;
    overflow-wrap: anywhere;
  }

  .s-info code {
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 12px;
  }

  .pill-offset {
    flex: none;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
  }
</style>
