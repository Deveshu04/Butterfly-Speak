<script lang="ts">
  import { onMount } from "svelte";
  import { formatBytes, systemInfo, type SystemInfo } from "$lib/api";
  import { update } from "$lib/update.svelte";

  let info = $state<SystemInfo | null>(null);

  onMount(() => {
    systemInfo().then((i) => (info = i));
    update.init();
  });
</script>

<div class="page">
  <div class="lockup">
    <svg class="lockup-mark" viewBox="0 0 24 24" aria-hidden="true">
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
      <circle class="lockup-lobe" cx="15.8" cy="16" r="3.6" />
      <g class="lockup-glyph" transform="translate(15.8 16)">
        <rect x="-1.6" y="-0.9" width="0.78" height="1.8" rx="0.39" />
        <rect x="-0.39" y="-1.75" width="0.78" height="3.5" rx="0.39" />
        <rect x="0.82" y="-1.2" width="0.78" height="2.4" rx="0.39" />
      </g>
    </svg>
    <h1 class="page-title">Butterfly <strong>Speak</strong></h1>
  </div>
  <p class="tagline">Dictation for India — English and 22 Indian languages.</p>
  {#if info}
    <p class="meta">
      Version {info.appVersion} · {formatBytes(info.totalRamBytes)} RAM detected
    </p>
  {/if}

  <section>
    <h2>Updates</h2>
    <div class="update-row">
      <p class="update-status">
        {#if update.state.kind === "checking"}
          Checking…
        {:else if update.state.kind === "upToDate"}
          You're up to date.
        {:else if update.state.kind === "available"}
          Version {update.state.version} is available.
        {:else if update.state.kind === "downloading"}
          Downloading{update.state.percent === null ? "…" : ` ${update.state.percent}%`}
        {:else if update.state.kind === "installing"}
          Restarting to finish the update…
        {:else if update.state.kind === "error"}
          <span class="update-error">{update.state.message}</span>
        {:else}
          Checks run half a minute after launch and every eight hours when enabled in Settings → System.
        {/if}
      </p>
      {#if update.state.kind === "available"}
        <button class="update-btn primary" onclick={() => update.install("about")}>Install and restart</button>
      {/if}
      <button
        class="update-btn"
        disabled={update.state.kind === "checking" ||
          update.state.kind === "downloading" ||
          update.state.kind === "installing"}
        onclick={() => update.check("about")}
      >
        Check for updates
      </button>
    </div>
    {#if update.state.kind === "available" && update.state.notes}
      <!-- Plain text on purpose: the manifest is fetched over the network and
           nothing it says may become markup here. -->
      <pre class="update-notes">{update.state.notes}</pre>
    {/if}
  </section>

  <section>
    <h2>Privacy</h2>
    <p>
      <strong>Cloud</strong> signs you in with Google and sends your audio, and
      the text for AI Polish, transforms, the voice agent, note actions,
      Auto-title and the prompt tester, through the Butterfly Labs relay to
      Sarvam AI; Butterfly Labs keeps your sign-in details and weekly usage
      counts, not your audio or text. <strong>Bring your own key</strong> sends
      them straight to Sarvam AI under your key, which lives in the Windows
      credential store. <strong>On-device</strong> runs speech recognition and
      AI Polish on this machine. With a Sarvam key saved, importing a recording
      and the translate shortcut go straight to Sarvam AI under your key in any
      mode; in On-device mode, so do transforms, the voice agent, note actions,
      Auto-title and the prompt tester. Your own AI endpoint, if you switch it
      on, receives what you switch it on for instead: your audio for
      speech-to-text; your text for transforms, the voice agent, note actions,
      Auto-title and the prompt tester; and your text for AI Polish only in the
      cloud modes or while it also transcribes (while it transcribes without
      AI Polish, Sarvam AI polishes instead: through the relay in Cloud mode,
      and under your saved key in the other modes). AI Polish receives your
      dictation after your corrections and snippets are applied, so it includes
      the text of any snippet you triggered. Your Dictionary words go, as
      spelling hints, with the audio you dictate wherever it is transcribed
      away from this machine, and with your text wherever AI Polish, the voice
      agent or the prompt tester runs away from it. There is no analytics or
      telemetry. Switch anytime in Settings → Speech engine.
    </p>
    <p>
      What leaves your computer in each mode, what Speak keeps on it and what
      uninstalling removes are set out in the privacy policy at
      deveshu04.github.io/privacy.html.
    </p>
  </section>

  <section>
    <h2>Built on</h2>
    <ul>
      <li><strong>Sarvam AI</strong> (Saaras speech-to-text, Sarvam-105B)</li>
      <li><strong>sherpa-onnx</strong> (Apache-2.0)</li>
      <li><strong>ONNX Runtime</strong> (MIT)</li>
      <li><strong>Moonshine</strong> (MIT)</li>
      <li><strong>NVIDIA Parakeet TDT-CTC 110M and TDT 0.6B v2</strong> (CC BY 4.0)</li>
      <li><strong>Edge-Punct-Casing punctuation model</strong> (Apache-2.0)</li>
      <li><strong>Qwen2.5-0.5B-Instruct</strong> (Apache-2.0)</li>
      <li><strong>llama.cpp</strong> (MIT)</li>
      <li><strong>Tauri</strong> (MIT/Apache-2.0)</li>
      <li><strong>Feather Icons</strong> (MIT)</li>
    </ul>
    <p>
      The full list, with the components these contain, is in
      THIRD_PARTY_NOTICES.md in the installation folder.
    </p>
  </section>

  <section>
    <h2>Why "Butterfly Speak"?</h2>
    <p>
      The butterfly effect: small changes, big consequences. Speak a sentence,
      watch it land anywhere — in any of the languages India speaks.
    </p>
  </section>
</div>

<style>
  /* The header lockup: mark at text height, "Butterfly" regular, the product
     name bold, the accent confined to the lobe. */
  .lockup {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 4px;
  }

  .lockup-mark {
    width: 34px;
    height: 34px;
    color: var(--fg);
    flex: none;
  }

  .lockup-lobe {
    fill: var(--brand-saffron);
  }

  .lockup-glyph {
    fill: var(--surface);
  }

  .page-title {
    margin-bottom: 0;
    font-weight: 450;
  }

  .page-title strong {
    font-weight: 700;
  }

  .tagline {
    font-family: var(--font-display);
    font-size: 16px;
    color: var(--fg-muted);
    margin: 4px 0 4px;
  }

  .meta {
    color: var(--fg-faint);
    font-size: 13px;
    margin: 0 0 24px;
  }

  section {
    margin-bottom: 24px;
  }

  h2 {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--fg-faint);
    margin: 0 0 8px;
  }

  p,
  li {
    color: var(--fg-muted);
    font-size: 13px;
    line-height: 1.6;
  }

  ul {
    margin: 0;
    padding-left: 18px;
  }

  strong {
    color: var(--fg);
    font-weight: 500;
  }

  .update-row {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }

  .update-status {
    flex: 1;
    min-width: 200px;
    margin: 0;
  }

  .update-error {
    color: var(--danger);
  }

  .update-btn {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 500;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    background: var(--surface);
    color: var(--fg);
    padding: 7px 14px;
    cursor: pointer;
  }

  .update-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .update-btn.primary {
    background: var(--accent);
    color: var(--accent-fg);
    border-color: transparent;
  }

  .update-notes {
    margin: 12px 0 0;
    padding: 12px 14px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    background: var(--bg);
    font-family: var(--font-ui);
    font-size: 12.5px;
    line-height: 1.55;
    color: var(--fg-muted);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 280px;
    overflow-y: auto;
  }
</style>
