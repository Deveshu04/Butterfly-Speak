<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { onDestroy, onMount } from "svelte";
  import {
    cloudSignIn,
    cloudSignOut,
    cloudStatus,
    listMics,
    sarvamKeyStatus,
    setMeter,
    validateSarvamKey,
    SIGNED_OUT_HERE_ONLY,
    type CloudStatus,
    type Provider,
  } from "$lib/api";
  import Dropdown, { type DropdownOption } from "$lib/components/Dropdown.svelte";
  import { CLOUD_AUTH_CHANGED, LEVEL, type LevelPayload } from "$lib/events";
  import { DASHBOARD_URL, LANGUAGES } from "$lib/sarvam";
  import { settings } from "$lib/stores.svelte";

  let step = $state(0);
  let mics = $state<string[]>([]);
  let level = $state(0);
  let playground = $state("");

  let keyInput = $state("");
  let keyPresent = $state(false);
  let validating = $state(false);
  let keyError = $state("");
  let saveError = $state("");

  let s = $derived(settings.current);
  let offlineMode = $derived(s?.provider === "local");

  /** The engine shown on the Engine step. A first run opens on Cloud (the
   * settings file's own default is Bring your own key, which is right for a
   * file that predates Cloud but wrong as a first suggestion); a re-run on an
   * install that has finished onboarding shows that install's real choice.
   * A first run that was left and resumed shows the engine already picked
   * when that is On-device or Cloud, which only a pick can have written;
   * Bring your own key is also the file's default, so it cannot be told from
   * no choice. Written back when the user picks an option or continues past
   * the step, so arriving there changes nothing. */
  const savedProvider = settings.current?.provider;
  let engine = $state<Provider>(
    settings.current?.app.onboardingDone
      ? (savedProvider ?? "cloud")
      : savedProvider === "local" || savedProvider === "cloud"
        ? savedProvider
        : "cloud",
  );
  let account = $state<CloudStatus | null>(null);
  let waitingForBrowser = $state(false);
  let cloudError = $state("");
  /** Settings' quiet sentence for a sign-out Supabase never heard; the
   * Cancel below goes through the same call. */
  let cloudNotice = $state("");

  /** The same three options the Settings picker offers, in the same order
   * and with the same words — two spellings of one choice would read as two
   * different choices. */
  const engineOptions: DropdownOption[] = [
    {
      value: "cloud",
      label: "Cloud",
      sublabel: "Sign in with Google · 2,000 words a week, free while in beta",
    },
    {
      value: "sarvam",
      label: "Bring your own key",
      sublabel: "English + 22 Indian languages, live",
    },
    { value: "local", label: "On-device", sublabel: "On this machine · English" },
  ];

  let micOptions = $derived<DropdownOption[]>([
    { value: "", label: "System default" },
    ...mics.map((m) => ({ value: m, label: m })),
  ]);

  // LANGUAGES labels look like "Hindi — हिन्दी": split into label + native
  // sublabel — matches GeneralSection's dictation-language picker.
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

  /** What the Try it step invites the user to speak. On-device models are
   * English only; the Sarvam lanes take the dictation language, or any of
   * the 23 when it is left on auto-detect. */
  let tryLanguages = $derived.by(() => {
    if (offlineMode) return "in English";
    const code = s?.sarvam.languageCode ?? "auto";
    const picked = LANGUAGES.find((l) => l.code === code);
    if (!picked || picked.code === "auto") return "in English, हिन्दी, or another Indian language";
    return `in ${picked.label.split(" — ").reverse()[0]}`;
  });

  const steps = ["Welcome", "Microphone", "Hotkey", "Engine", "Try it"];

  /** Asked about only while the Engine step shows Cloud, the one place the
   * account appears: the first ask after a launch renews a stored sign-in
   * with Supabase, and nothing before that step may. */
  let cloudChosen = $derived(engine === "cloud" && steps[step] === "Engine");

  $effect(() => {
    if (cloudChosen) cloudStatus().then((st) => (account = st));
  });

  onMount(() => {
    listMics().then((m) => (mics = m));
    sarvamKeyStatus().then((st) => (keyPresent = st.present));
    const unsubs: Array<() => void> = [];
    listen<LevelPayload>(LEVEL, (e) => {
      level = Math.min(1, e.payload.level * 9);
    }).then((u) => unsubs.push(u));
    listen<CloudStatus>(CLOUD_AUTH_CHANGED, (e) => {
      account = e.payload;
      waitingForBrowser = false;
    }).then((u) => unsubs.push(u));
    return () => unsubs.forEach((u) => u());
  });

  onDestroy(() => setMeter(false));

  function next() {
    step += 1;
    if (steps[step] === "Microphone") setMeter(true);
    if (steps[step] === "Hotkey") setMeter(false);
  }

  async function validateKey() {
    keyError = "";
    validating = true;
    try {
      await validateSarvamKey(keyInput);
      // Persist the provider before flipping the UI — a failed settings
      // write must keep the error visible on this branch (and keep the
      // pasted key around for retry).
      await settings.update((st) => (st.provider = "sarvam"));
      keyPresent = true;
      keyInput = "";
    } catch (e) {
      keyError = String(e);
    } finally {
      validating = false;
    }
  }

  /** Write the chosen engine. Nothing is downloaded here or on the switch —
   * on-device models are fetched only via the explicit Download buttons in
   * Settings → Speech engine. */
  async function persistEngine(): Promise<boolean> {
    saveError = "";
    try {
      await settings.update((st) => (st.provider = engine));
      return true;
    } catch (e) {
      saveError = String(e);
      return false;
    }
  }

  function chooseEngine(v: string) {
    engine = v as Provider;
    persistEngine();
  }

  async function continueEngine() {
    if (await persistEngine()) next();
  }

  async function signIn() {
    cloudError = "";
    cloudNotice = "";
    waitingForBrowser = true;
    try {
      await cloudSignIn();
    } catch (e) {
      cloudError = String(e);
      waitingForBrowser = false;
    }
  }

  /** Cancels a sign-in still out at the browser; the backend treats a
   * sign-out and a cancelled trip as the same thing, so a sign-in stored
   * meanwhile is signed out too, and said so if Supabase couldn't be
   * reached. */
  async function cancelSignIn() {
    cloudError = "";
    cloudNotice = "";
    try {
      if ((await cloudSignOut()) === "hereOnly") cloudNotice = SIGNED_OUT_HERE_ONLY;
    } catch (e) {
      cloudError = String(e);
    } finally {
      waitingForBrowser = false;
    }
  }

  async function finish() {
    saveError = "";
    try {
      await settings.update((st) => (st.app.onboardingDone = true));
    } catch (e) {
      saveError = String(e);
    }
  }
</script>

<div class="onboarding">
  <div class="progress-dots">
    {#each steps as _, i}
      <span class="dot" class:active={i === step} class:done={i < step}></span>
    {/each}
  </div>

  {#if step === 0}
    <div class="step">
      <h1>Butterfly Speak</h1>
      <p class="tagline">Dictate in your language — English and 22 Indian languages.</p>
      <p>
        Dictate into any app: hold a key, speak, release — your words are typed
        where your cursor is. Powered by Sarvam AI's Indian-language speech
        models, with an on-device mode that recognises English speech on this
        machine if you prefer.
      </p>
      <button onclick={next}>Set up</button>
    </div>
  {:else if step === 1}
    <div class="step">
      <h1>Microphone</h1>
      <p>Pick a microphone and say something — the bar should move.</p>
      {#if s}
        <Dropdown
          options={micOptions}
          value={s.audio.deviceName ?? ""}
          onchange={(v) => settings.update((st) => (st.audio.deviceName = v || null))}
          ariaLabel="Microphone"
        />
      {/if}
      <div class="meter">
        <div class="meter-fill" style="width: {level * 100}%"></div>
      </div>
      <p class="small">
        Nothing moving? Check Windows Settings → Privacy &amp; security → Microphone.
      </p>
      <button onclick={next}>Continue</button>
    </div>
  {:else if step === 2}
    <div class="step">
      <h1>Your hotkey</h1>
      <p>
        Hold <kbd>{s?.hotkey.binding ?? "Ctrl+Win"}</kbd> to dictate, release to
        type. Double-tap it for hands-free mode; tap again to finish.
      </p>
      <p class="small">
        You can change the combination later in Settings → General → Shortcuts.
      </p>
      <button onclick={next}>Continue</button>
    </div>
  {:else if step === 3}
    <div class="step">
      <h1>Choose your engine</h1>
      <p>You can change this later in Settings → Speech engine.</p>
      <Dropdown
        options={engineOptions}
        value={engine}
        onchange={chooseEngine}
        ariaLabel="Engine"
      />

      {#if engine === "cloud"}
        <p class="small">
          Your audio, and the text for AI features such as AI Polish, go through
          the Butterfly Labs relay to Sarvam AI. Butterfly Labs keeps your
          sign-in details and weekly usage counts, not your audio or text.
        </p>
        {#if account?.signedIn}
          <p class="ready">Signed in{account.email ? ` as ${account.email}` : ""}.</p>
        {:else if account === null && !waitingForBrowser}
          <p class="small">Checking your sign-in…</p>
        {:else if waitingForBrowser}
          <p class="small">Waiting for your browser…</p>
          <button class="secondary" onclick={cancelSignIn}>Cancel</button>
        {:else}
          <button class="secondary" onclick={signIn}>Sign in with Google</button>
          <p class="small">
            Sign-in opens in your browser. You can do it later from Settings.
          </p>
          {#if cloudNotice}
            <p class="small">{cloudNotice}</p>
          {/if}
        {/if}
        {#if cloudError}
          <p class="error">{cloudError}</p>
        {/if}
        <button onclick={continueEngine}>Continue</button>
        {#if saveError}
          <p class="error">{saveError}</p>
        {/if}
      {:else if engine === "sarvam"}
        <p>
          Butterfly Speak uses your own Sarvam account for speech recognition —
          you stay in control of your key and your usage. New accounts include
          free credits.
        </p>
        {#if !keyPresent}
          <button class="secondary" onclick={() => openUrl(DASHBOARD_URL)}>
            Get a free API key ↗
          </button>
          <input
            type="password"
            placeholder="Paste your API key"
            bind:value={keyInput}
            onkeydown={(e) => {
              if (e.key === "Enter" && keyInput.trim() && !validating) validateKey();
            }}
          />
          {#if keyError}
            <p class="error">{keyError}</p>
          {/if}
          <button onclick={validateKey} disabled={!keyInput.trim() || validating}>
            {validating ? "Checking…" : "Validate key"}
          </button>
          {#if saveError}
            <p class="error">{saveError}</p>
          {/if}
        {:else}
          <p class="ready">Connected. Pick your dictation language:</p>
          {#if s}
            <Dropdown
              options={languageOptions}
              value={s.sarvam.languageCode}
              onchange={(v) => settings.update((st) => (st.sarvam.languageCode = v))}
              ariaLabel="Dictation language"
            />
          {/if}
          <p class="small">
            Auto-detect handles mixed speech well; picking one language is a bit
            faster and more accurate.
          </p>
          <button onclick={continueEngine}>Continue</button>
          {#if saveError}
            <p class="error">{saveError}</p>
          {/if}
        {/if}
      {:else}
        <p>
          Speech is recognised on this machine, with no account. English only.
        </p>
        <p class="small">
          Download the models from Settings → Speech engine when you're ready.
        </p>
        <button onclick={continueEngine}>Continue</button>
        {#if saveError}
          <p class="error">{saveError}</p>
        {/if}
      {/if}
    </div>
  {:else}
    <div class="step">
      <h1>Try it</h1>
      {#if offlineMode}
        <p class="verdict">
          On-device mode: use the Download button in Settings → Speech engine to
          fetch the required models before dictating.
        </p>
      {/if}
      <p>
        Click into the box below, hold
        <kbd>{s?.hotkey.binding ?? "Ctrl+Win"}</kbd>, and say something —
        {tryLanguages}.
      </p>
      <textarea
        bind:value={playground}
        placeholder="Dictate here…"
        rows="4"
      ></textarea>
      <button onclick={finish} class="finish">Start using Butterfly Speak</button>
      {#if saveError}
        <p class="error">{saveError}</p>
      {/if}
      <p class="small">
        The app lives in your system tray. Closing the window keeps dictation
        running.
      </p>
    </div>
  {/if}
</div>

<style>
  .onboarding {
    max-width: 480px;
    margin: 0 auto;
    padding: 28px 24px 48px;
  }

  .progress-dots {
    display: flex;
    gap: 8px;
    margin-bottom: 36px;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--hairline);
    transition: background var(--motion);
  }

  .dot.active {
    background: var(--fg);
  }

  .dot.done {
    background: var(--fg-faint);
  }

  h1 {
    font-size: 26px;
    font-weight: 650;
    letter-spacing: -0.02em;
    margin: 0 0 6px;
  }

  .tagline {
    color: var(--fg-muted);
    margin: 0 0 16px;
  }

  p {
    line-height: 1.6;
    color: var(--fg-muted);
  }

  button {
    font-family: var(--font-ui);
    font-size: 14px;
    font-weight: 600;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--accent-fg);
    border-radius: var(--radius-control);
    padding: 10px 22px;
    cursor: pointer;
    margin-top: 16px;
  }

  button:disabled {
    opacity: 0.5;
    cursor: default;
  }

  input[type="password"] {
    font-family: var(--font-ui);
    font-size: 14px;
    padding: 9px 12px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    background: var(--surface);
    color: var(--fg);
    width: 100%;
    margin-top: 8px;
  }

  input[type="password"] {
    margin-top: 14px;
  }

  .meter {
    margin-top: 14px;
    height: 8px;
    border-radius: 999px;
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    overflow: hidden;
  }

  .meter-fill {
    height: 100%;
    background: var(--accent);
    transition: width 80ms linear;
  }

  .small {
    font-size: 13px;
    color: var(--fg-faint);
  }

  .ready {
    color: var(--fg);
    font-weight: 500;
  }

  .verdict {
    font-size: 13px;
    color: var(--fg-muted);
    border-left: 2px solid var(--hairline);
    padding-left: 10px;
  }

  .error {
    font-size: 13px;
    color: var(--danger);
    margin: 8px 0 0;
  }

  button.secondary {
    background: var(--bg-elevated);
    border-color: var(--hairline);
    color: var(--fg);
    margin-top: 4px;
  }

  textarea {
    width: 100%;
    font-family: var(--font-ui);
    font-size: 14px;
    line-height: 1.6;
    padding: 12px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    background: var(--bg-elevated);
    color: var(--fg);
    resize: vertical;
    user-select: text;
    cursor: text;
  }

  .finish {
    display: block;
  }
</style>
