<script lang="ts">
  import { onMount } from "svelte";
  import { listMics } from "$lib/api";
  import { LANGUAGES } from "$lib/sarvam";
  import { settings } from "$lib/stores.svelte";
  import Dropdown, { type DropdownOption } from "$lib/components/Dropdown.svelte";
  import ShortcutsDialog from "./ShortcutsDialog.svelte";
  import Switch from "./Switch.svelte";
  import "./rows.css";

  let s = $derived(settings.current);

  let mics = $state<string[]>([]);
  let shortcutsOpen = $state(false);

  let micOptions = $derived<DropdownOption[]>([
    { value: "", label: "System default" },
    ...mics.map((m) => ({ value: m, label: m })),
  ]);

  // LANGUAGES labels look like "Hindi — हिन्दी": split into label + native
  // sublabel.
  let languageOptions = $derived<DropdownOption[]>(
    LANGUAGES.map((l) => {
      const [label, sublabel] = l.label.split(" — ");
      return {
        value: l.code,
        label,
        sublabel: sublabel ?? (l.code === "auto" ? "All 23 languages" : undefined),
      };
    }),
  );

  // The translate chord's target. Same list, same rendering as the dictation
  // picker above — deliberately derived from it rather than rebuilt, so one
  // language can never read two different ways in the same panel — minus
  // `auto`: `/translate` has no auto-detect and rejects "auto" as a target.
  // What is left is exactly `sarvam-translate:v1`'s 23 languages; Odia is
  // spelled `or-IN` here (the realtime vocabulary this list speaks) and
  // rewritten to `od-IN` on the wire by
  // `sarvam::translate::to_translate_language_code`. A closed set is the
  // point: `target_language` goes to Sarvam verbatim.
  let targetOptions = $derived<DropdownOption[]>(languageOptions.filter((o) => o.value !== "auto"));

  // Mirrors `routes::translate::source_language`: the values that mean "the
  // app does not actually know what language you are speaking".
  // `sarvam-translate:v1` cannot detect it, so the route skips the
  // translation and pastes the cleaned text — the row says so before the
  // user finds out from the pill.
  let sourceUnset = $derived(
    ["", "auto", "unknown"].includes((s?.sarvam.languageCode ?? "").trim().toLowerCase()),
  );

  // No chord, no translation — the setting below is inert until one is bound.
  let translateChordUnbound = $derived(!s?.shortcuts.translateDictation.trim());

  const latencyOptions: DropdownOption[] = [
    { value: "balanced", label: "Balanced", sublabel: "Recommended" },
    { value: "fast", label: "Fast", sublabel: "Lower latency, slightly less accurate" },
  ];

  onMount(() => {
    listMics().then((m) => (mics = m));
  });
</script>

{#if s}
  <h1 class="s-title">General</h1>

  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Shortcuts</p>
        <p class="s-row-sub">
          Hold
          {#each s.hotkey.binding.split("+") as key, i}{#if i > 0}{" + "}{/if}<kbd
            >{key}</kbd
          >{/each}
          and speak.
        </p>
      </div>
      <button class="s-btn" onclick={() => (shortcutsOpen = true)}>Change</button>
    </div>

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Microphone</p>
        <p class="s-row-sub">Which input Butterfly Speak listens to.</p>
      </div>
      <Dropdown
        options={micOptions}
        value={s.audio.deviceName ?? ""}
        onchange={(v) => settings.update((st) => (st.audio.deviceName = v || null))}
      />
    </div>

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-audio-cues">Audio cues</p>
        <p class="s-row-sub">A short tone when a recording starts and stops.</p>
      </div>
      <Switch
        labelledby="set-audio-cues"
        checked={s.audio.cues}
        onchange={(on) => settings.update((st) => (st.audio.cues = on))}
      />
    </div>

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-pause-media">Pause media while dictating</p>
        <p class="s-row-sub">Pauses music or video for the recording, then resumes it.</p>
      </div>
      <Switch
        labelledby="set-pause-media"
        checked={s.audio.pauseMedia}
        onchange={(on) => settings.update((st) => (st.audio.pauseMedia = on))}
      />
    </div>

    <!-- Deliberately outside the cloud-only block below. The voice agent runs
         with every engine: through the Butterfly Labs relay on Cloud, under
         the saved Sarvam key otherwise, or on the user's own AI endpoint when
         that is on for AI Polish. So this setting keeps working after a
         switch to the on-device engine, and hiding the switch there would
         strand a user who turned it on with dictations that type nothing and
         no way back to the toggle. -->
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-wake-word">Wake word</p>
        <!-- Three things a user has to know before turning this on, and all
             three are in the copy: it is new, it listens on the ordinary
             dictation chord, and a command the agent can't serve costs them
             the dictation rather than leaving a stray paragraph to delete.
             "Experimental" is not hedging — on a name this long the matcher
             forgives a misheard letter on top of the spellings it evens out,
             which is a real false-positive surface. -->
        <p class="s-row-sub">
          Experimental. Open an ordinary dictation with “{s.agent.name}” and the rest of
          it goes to the voice agent instead of your document. Nothing is typed if the agent
          can't run — a command is never pasted as text.
        </p>
      </div>
      <Switch
        labelledby="set-wake-word"
        checked={s.agent.wakeWordEnabled}
        onchange={(on) => settings.update((st) => (st.agent.wakeWordEnabled = on))}
      />
    </div>

    <!-- Both cloud engines reach the same Sarvam models, so these rows belong
         to either of them; only the on-device engine has no use for them.
         Spelled out rather than "not local" so that a fourth provider has to
         opt into these rows instead of inheriting them. -->
    {#if s.provider === "sarvam" || s.provider === "cloud"}
      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title">Dictation language</p>
          <p class="s-row-sub">Auto-detect handles mixed speech well.</p>
        </div>
        <Dropdown
          options={languageOptions}
          value={s.sarvam.languageCode}
          onchange={(v) => settings.update((st) => (st.sarvam.languageCode = v))}
        />
      </div>

      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title">Translate into</p>
          <!-- Two preconditions gate this setting, and only the first one the
               user can act on is shown: fixing the chord reveals the language
               line if that is still unset. Both at once is a three-line row
               and two things to do; this is one. -->
          <p class="s-row-sub">
            Dictate in your set language — the translate chord pastes in this one.
            {#if translateChordUnbound}
              Bind that chord under Shortcuts to use it.
            {:else if sourceUnset}
              Set a dictation language above — Auto-detect can't be translated.
            {/if}
          </p>
        </div>
        <Dropdown
          options={targetOptions}
          value={s.translation.targetLanguage}
          onchange={(v) => settings.update((st) => (st.translation.targetLanguage = v))}
        />
      </div>

      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title">Latency</p>
          <p class="s-row-sub">How eagerly transcription streams back.</p>
        </div>
        <Dropdown
          options={latencyOptions}
          value={s.sarvam.streamType}
          onchange={(v) =>
            settings.update((st) => (st.sarvam.streamType = v as "fast" | "balanced"))}
        />
      </div>
    {/if}
  </div>

  {#if shortcutsOpen}
    <ShortcutsDialog onclose={() => (shortcutsOpen = false)} />
  {/if}
{/if}
