<script lang="ts">
  import { onMount } from "svelte";
  import {
    checkCustomEndpoint,
    customEndpointKeyStatus,
    endpointListModels,
    endpointTestConnection,
    setCustomEndpointKey,
    type CustomEndpointKeyStatus,
    type EndpointModel,
  } from "$lib/api";
  import Dropdown, { type DropdownOption } from "$lib/components/Dropdown.svelte";
  import { settings } from "$lib/stores.svelte";
  import Switch from "./Switch.svelte";
  import "./rows.css";

  let s = $derived(settings.current);
  let ce = $derived(s?.customEndpoint);

  // Drafts, not bindings. A URL is unusable for most of the time it is being
  // typed, so the settings write happens when the field is left — never on
  // every keystroke, which would rewrite a half-typed host under the cursor
  // and re-run the pre-flight against nonsense.
  let urlDraft = $state("");
  let modelDraft = $state("");
  let sttModelDraft = $state("");

  /** The full chat route the backend says it would request, or "". Comes
   * from Rust rather than being assembled here: `/v1` is appended only when
   * missing, and a base carrying `?api-version=…` keeps its query at the end
   * — neither survives string concatenation in the webview. */
  let chatRoute = $state("");
  /** The pre-flight sentence for an unusable URL. Empty when fine. */
  let urlError = $state("");

  let keyStatus = $state<CustomEndpointKeyStatus | null>(null);
  let keyDraft = $state("");
  let editingKey = $state(false);
  let keyError = $state("");
  let savingKey = $state(false);
  /** Editor is open while explicitly editing, or whenever no key is stored —
   * the same rule the Sarvam key row uses. */
  let keyOpen = $derived(editingKey || (keyStatus !== null && !keyStatus.present));

  let models = $state<EndpointModel[]>([]);
  let listNote = $state("");
  let listError = $state("");
  let loadingModels = $state(false);

  type TestVerdict = { ok: boolean; text: string } | null;

  let testing = $state(false);
  let testResult = $state<TestVerdict>(null);

  /** Why polish is going to Sarvam even though this endpoint is switched on.
   * Empty when it isn't. */
  let fallbackReason = $state("");

  onMount(() => {
    const current = settings.current?.customEndpoint;
    urlDraft = current?.baseUrl ?? "";
    modelDraft = current?.model ?? "";
    sttModelDraft = current?.sttModel ?? "";
    preflight(urlDraft);
    refreshFallback();
    customEndpointKeyStatus().then((k) => (keyStatus = k));
  });

  /** Pure pre-flight — no network call, nothing stored. Runs on every commit
   * so the field can be honest about a URL before anything is sent to it. */
  async function preflight(url: string) {
    if (!url.trim()) {
      chatRoute = "";
      urlError = "";
      return;
    }
    try {
      chatRoute = await checkCustomEndpoint(url);
      urlError = "";
    } catch (e) {
      chatRoute = "";
      urlError = String(e);
    }
  }

  /** The two conditions under which `endpoint::resolve` degrades an enabled
   * slot back to Sarvam, in its order. Pinned Rust-side by
   * `an_enabled_slot_degrades_only_for_a_missing_model_or_an_unusable_base`:
   * a third reason added there without a line here would leave this panel
   * looking healthy over a Sarvam request. */
  async function refreshFallback() {
    const current = settings.current?.customEndpoint;
    if (!current?.useForPolish) {
      fallbackReason = "";
      return;
    }
    if (!current.model.trim()) {
      fallbackReason = "no model id set.";
      return;
    }
    try {
      await checkCustomEndpoint(current.baseUrl);
      fallbackReason = "";
    } catch (e) {
      fallbackReason = String(e);
    }
  }

  /** Drop the last test verdict — unless a newer one arrived while we were
   * awaiting.
   *
   * Clicking "Test" blurs whichever field had focus, so a commit and a probe
   * are routinely in flight at the same time and the commit's settings
   * round-trip is often the slower of the two. Wiping unconditionally at the
   * end of it erases the answer the user just asked for, a moment after it
   * appeared. Comparing identity rather than checking a `testing` flag covers
   * both orderings: a probe that has already landed holds a different object,
   * and one still running has set this to `null` on its way out. */
  function invalidateTestResult(stale: TestVerdict) {
    if (testResult === stale) testResult = null;
  }

  /** The URL save in flight, if any. Both probes wait for it: clicking Test
   * or Load leaves the field, and the backend sends the stored key only to
   * the saved address (`endpoint::key_for_probe`), so the probe must not
   * reach it before the save does. */
  let urlCommit: Promise<void> = Promise.resolve();

  function commitUrl(): Promise<void> {
    urlCommit = saveUrl();
    return urlCommit;
  }

  async function saveUrl() {
    const next = urlDraft.trim();
    urlDraft = next;
    if (next !== (ce?.baseUrl ?? "")) {
      const stale = testResult;
      await settings.update((st) => (st.customEndpoint.baseUrl = next));
      // A different host knows nothing about the last host's answers.
      models = [];
      listNote = "";
      listError = "";
      invalidateTestResult(stale);
    }
    await preflight(next);
    await refreshFallback();
  }

  async function commitModel() {
    const next = modelDraft.trim();
    modelDraft = next;
    if (next === (ce?.model ?? "")) return;
    await settings.update((st) => (st.customEndpoint.model = next));
    await refreshFallback();
  }

  /** The transcription half's own id. Deliberately never defaulted to
   * `model`: that one names a chat model, and posting it to
   * `/audio/transcriptions` is a 400 from every server. Left empty, Rust
   * sends `whisper-1`, OpenAI's id for its Whisper model, which servers
   * that copy OpenAI's transcription route usually accept. */
  async function commitSttModel() {
    const next = sttModelDraft.trim();
    sttModelDraft = next;
    if (next === (ce?.sttModel ?? "")) return;
    await settings.update((st) => (st.customEndpoint.sttModel = next));
  }

  /** Picking from the listed ids is just another way of typing the id —
   * it writes the same field, and nothing else on this screen may write it. */
  async function pickModel(id: string) {
    modelDraft = id;
    await commitModel();
  }

  async function commitKey() {
    // Leaving an untouched field is not a request to change the credential.
    if (!keyDraft.trim()) {
      keyDraft = "";
      if (keyStatus?.present) editingKey = false;
      return;
    }
    savingKey = true;
    keyError = "";
    const stale = testResult;
    try {
      await setCustomEndpointKey(keyDraft);
      keyDraft = "";
      editingKey = false;
      keyStatus = await customEndpointKeyStatus();
      // A new credential makes the last verdict stale, whatever it said.
      invalidateTestResult(stale);
    } catch (e) {
      keyError = String(e);
    } finally {
      savingKey = false;
    }
  }

  async function clearKey() {
    savingKey = true;
    keyError = "";
    const stale = testResult;
    try {
      await setCustomEndpointKey("");
      keyDraft = "";
      editingKey = false;
      keyStatus = await customEndpointKeyStatus();
      invalidateTestResult(stale);
    } catch (e) {
      keyError = String(e);
    } finally {
      savingKey = false;
    }
  }

  /** The URL a probe should use: whatever is in the field right now, so the
   * button answers for what the user is looking at rather than for whatever
   * was last saved. */
  let probeTarget = $derived(urlDraft.trim() || (ce?.baseUrl ?? ""));

  async function loadModels() {
    loadingModels = true;
    listError = "";
    listNote = "";
    try {
      await urlCommit;
      models = await endpointListModels(probeTarget);
      if (models.length === 0) {
        listNote =
          "The endpoint answered but listed no models. Type the id by hand — it may still work.";
      }
    } catch (e) {
      models = [];
      listError = String(e);
    } finally {
      loadingModels = false;
    }
  }

  async function testConnection() {
    testing = true;
    testResult = null;
    listError = "";
    try {
      await urlCommit;
      const probed = await endpointTestConnection(probeTarget);
      // A successful test has already fetched the list; throwing it away just
      // to make the user press the other button would be silly.
      models = probed.models;
      const n = probed.models.length;
      testResult = {
        ok: true,
        text:
          n > 0
            ? `Reached ${probed.url} — ${n} model${n === 1 ? "" : "s"} offered.`
            : `Reached ${probed.url}, but it listed no models. The id you type by hand may still work.`,
      };
    } catch (e) {
      testResult = { ok: false, text: String(e) };
    } finally {
      testing = false;
    }
  }

  /** The ids the endpoint listed, with the user's own id kept among them.
   *
   * `/models` only suggests ids. An id it left out may still work (a model
   * the server loads on demand, or one it does not advertise), so the user's
   * id stays selectable and stays selected. Nothing in this component ever
   * writes an empty string to the model field. */
  let pickerOptions = $derived.by<DropdownOption[]>(() => {
    const options: DropdownOption[] = models.map((m) => ({
      value: m.id,
      label: m.id,
      sublabel: m.ownedBy ? `Served by ${m.ownedBy}` : undefined,
    }));
    const chosen = (ce?.model ?? "").trim();
    if (chosen && !options.some((o) => o.value === chosen)) {
      options.unshift({
        value: chosen,
        label: chosen,
        sublabel: "Your id — the endpoint didn't list it",
      });
    }
    return options;
  });

  let polishOn = $derived(ce?.useForPolish === true);
</script>

{#if ce}
  <h1 class="s-title">Custom endpoint</h1>

  <p class="lead">
    Point the writing half of Butterfly Speak at your own OpenAI-compatible server —
    LM Studio, Ollama, vLLM, llama-server, or a hosted gateway — instead of Sarvam.
    Speech stays with your speech engine unless you also turn on Use for
    speech-to-text below, which sends your dictation audio to this server too.
  </p>

  <p class="s-group">Endpoint</p>
  <div class="s-panel">
    <div class="s-row stack">
      <div class="s-info">
        <p class="s-row-title">Base URL</p>
        <p class="s-row-sub">
          The base must serve <code>/v1/chat/completions</code>. <code>/v1</code> is
          appended for you <em>on the chat route</em>, so
          <code>http://localhost:11434</code> and <code>http://localhost:11434/v1</code>
          reach the same place — and pasting a full <code>…/v1/chat/completions</code>
          URL out of a curl example works too. Plain <code>http://</code> is allowed
          only for addresses on your own machine or local network; everything else must
          be <code>https://</code>.
        </p>
      </div>
      <input
        class="s-input wide"
        type="url"
        spellcheck="false"
        autocomplete="off"
        placeholder="http://localhost:11434"
        aria-label="Endpoint base URL"
        bind:value={urlDraft}
        onblur={commitUrl}
        onkeydown={(e) => {
          if (e.key === "Enter") e.currentTarget.blur();
        }}
      />
      {#if urlError}
        <p class="s-error">{urlError}</p>
      {:else if chatRoute}
        <p class="s-note">Requests go to <code>{chatRoute}</code>.</p>
      {/if}
    </div>

    <div class="s-row stack">
      <div class="key-head">
        <div class="s-info">
          <p class="s-row-title">API key</p>
          <p class="s-row-sub">
            {#if keyStatus?.present}
              Stored as {keyStatus.masked} — sent as a Bearer token, and only to this
              endpoint.
            {:else}
              Optional. Many self-hosted servers take no auth at all; leave this empty
              and nothing is sent. Stored in the Windows credential store, never in the
              settings file, and never included in an export.
            {/if}
          </p>
        </div>
        <!-- Gated on the status having arrived, not just on `keyOpen`: until
             it has, "no key stored" and "not asked yet" look the same, and
             offering to Remove a key that isn't there is the wrong guess. -->
        {#if keyStatus !== null && !keyOpen}
          <button class="s-btn" onclick={() => (editingKey = true)}>Change</button>
          <button class="s-btn" disabled={savingKey} onclick={clearKey}>Remove</button>
        {/if}
      </div>
      {#if keyOpen}
        <div class="key-edit">
          <input
            class="s-input wide"
            type="password"
            autocomplete="off"
            placeholder="Paste the endpoint's API key (optional)"
            aria-label="Endpoint API key"
            bind:value={keyDraft}
            onblur={commitKey}
            onkeydown={(e) => {
              if (e.key === "Enter") e.currentTarget.blur();
              if (e.key === "Escape" && keyStatus?.present) {
                e.stopPropagation();
                keyDraft = "";
                editingKey = false;
              }
            }}
          />
          {#if savingKey}
            <span class="s-value">Saving…</span>
          {/if}
        </div>
        <p class="s-note">Saved when you click away or press Enter.</p>
      {/if}
      {#if keyError}
        <p class="s-error">{keyError}</p>
      {/if}
    </div>

    <div class="s-row stack">
      <div class="s-info">
        <p class="s-row-title">Model</p>
        <p class="s-row-sub">
          The id to send, exactly as your server spells it — <code>qwen3:8b</code>,
          <code>llama-3.1-8b-instruct</code>, whatever it is. The list below is a
          suggestion drawn from the endpoint's own <code>/models</code>; an id it
          doesn't advertise is still yours to use.
        </p>
      </div>
      <div class="model-edit">
        <input
          class="s-input wide"
          type="text"
          spellcheck="false"
          autocomplete="off"
          placeholder="model-id"
          aria-label="Model id"
          bind:value={modelDraft}
          onblur={commitModel}
          onkeydown={(e) => {
            if (e.key === "Enter") e.currentTarget.blur();
          }}
        />
        <button class="s-btn" disabled={loadingModels || !probeTarget} onclick={loadModels}>
          {loadingModels ? "Loading…" : models.length ? "Reload list" : "Load list"}
        </button>
      </div>
      {#if models.length > 0}
        <div class="model-pick">
          <span class="s-value">Discovered:</span>
          <Dropdown
            options={pickerOptions}
            value={ce.model}
            onchange={pickModel}
            placeholder="Choose a model"
            ariaLabel="Models offered by the endpoint"
          />
        </div>
      {/if}
      {#if listError}
        <p class="s-error">{listError}</p>
      {:else if listNote}
        <p class="s-note">{listNote}</p>
      {/if}
    </div>

    <div class="s-row stack">
      <div class="key-head">
        <div class="s-info">
          <p class="s-row-title">Test connection</p>
          <p class="s-row-sub">
            Asks the endpoint for its model list, with a four-second budget. Nothing is
            saved and no dictation is sent.
          </p>
        </div>
        <button
          class="s-btn primary"
          disabled={testing || !probeTarget}
          onclick={testConnection}
        >
          {testing ? "Testing…" : "Test"}
        </button>
      </div>
      {#if testResult}
        <p class={testResult.ok ? "s-note ok" : "s-error"}>{testResult.text}</p>
      {/if}
    </div>
  </div>

  <p class="s-group">Where it's used</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-endpoint-polish">Use for AI Polish</p>
        <p class="s-row-sub">
          Sends formatting, transforms and the voice agent to this endpoint instead of
          Sarvam. Your transcript leaves for whatever host you named above, so name one
          you trust.
        </p>
      </div>
      <Switch
        labelledby="set-endpoint-polish"
        checked={ce.useForPolish}
        onchange={async (on) => {
          await settings.update((st) => (st.customEndpoint.useForPolish = on));
          await refreshFallback();
        }}
      />
    </div>

    {#if polishOn && fallbackReason}
      <!-- The backend degrades an enabled-but-unusable endpoint back to
           Sarvam and logs why. A log the user cannot read is not an answer to
           "I switched this on and nothing changed", so the same reason is
           said here.

           It only degrades while Sarvam is already hearing the dictation.
           With speech-to-text pointed here too, Sarvam has heard nothing, and
           quietly sending it the transcript would be the surprise the
           fallback exists to avoid — so the words paste unpolished instead,
           with a notice. -->
      <div class="s-row">
        {#if ce.useForStt}
          <p class="s-error fallback">
            Not usable, and speech-to-text is pointed here too — so nothing goes to
            Sarvam instead: {fallbackReason} Dictations still paste, as dictated.
          </p>
        {:else}
          <p class="s-error fallback">Falling back to Sarvam: {fallbackReason}</p>
        {/if}
      </div>
    {/if}

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-endpoint-stt">Use for speech-to-text</p>
        <p class="s-row-sub">
          Records the whole utterance and posts it to
          <code>/audio/transcriptions</code> on the host above, instead of streaming to
          Sarvam or running the on-device model. Your voice leaves for whatever host you
          named, so name one you trust. Your dictionary goes with it, as a hint. The
          translate shortcut still goes to Sarvam — that one is a Sarvam-only endpoint,
          not a chat model, so nothing else can stand in for it.
          <strong><code>/v1</code> is not added on this route</strong>: if your server
          answers at <code>/v1/audio/transcriptions</code> — speaches,
          faster-whisper-server, LocalAI and LM Studio all do — include
          <code>/v1</code> in the URL above.
        </p>
      </div>
      <!-- A `useForStt` that arrives from a settings import or a hand-edited
           file is HONOURED rather than reset: quietly undoing a choice the
           user made is the behaviour `repair()` reserves for values that are
           *invalid*. An unconfigured endpoint cannot leak anything by being
           honoured, either — `endpoint::resolve_stt` fails before a byte is
           sent, and the first dictation says so in the pill. -->
      <Switch
        labelledby="set-endpoint-stt"
        checked={ce.useForStt}
        onchange={(on) => settings.update((st) => (st.customEndpoint.useForStt = on))}
      />
    </div>

    {#if ce.useForStt}
      <div class="s-row stack">
        <div class="s-info">
          <p class="s-row-title">Transcription model</p>
          <p class="s-row-sub">
            The id your server answers to on the transcription route —
            <code>whisper-large-v3</code>, a deployment name, whatever it is. Separate
            from the chat model above, because one host rarely uses the same name for
            both. Left empty, <code>whisper-1</code> is sent: it is OpenAI's name for its
            Whisper model, and servers built to answer like OpenAI's usually accept it.
          </p>
        </div>
        <input
          class="s-input wide"
          type="text"
          spellcheck="false"
          autocomplete="off"
          placeholder="whisper-1"
          aria-label="Transcription model id"
          bind:value={sttModelDraft}
          onblur={commitSttModel}
          onkeydown={(e) => {
            if (e.key === "Enter") e.currentTarget.blur();
          }}
        />
      </div>
    {/if}
  </div>
{/if}

<style>
  .lead {
    font-size: 13.5px;
    color: var(--fg-muted);
    line-height: 1.55;
    margin: -14px 0 26px;
    max-width: 64ch;
  }

  /* Rows whose control needs the full width sit under the title/sub rather
     than beside it — the same shape EngineSection's key row uses. */
  .s-row.stack {
    flex-direction: column;
    align-items: stretch;
    gap: 0;
  }

  .key-head {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .key-head .s-info {
    margin-right: 12px;
  }

  .key-edit,
  .model-edit {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 12px;
  }

  .model-pick {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 10px;
  }

  /* `.s-input` caps at 260px in rows.css — right for a control sitting beside
     its label, wrong for a URL. Scoped, not `:global`: nothing outside this
     section should inherit the override. */
  .s-input.wide {
    flex: 1;
    max-width: none;
  }

  .s-row.stack > input.s-input {
    margin-top: 12px;
  }

  .s-note.ok {
    color: var(--teal);
  }

  .fallback {
    margin: 0;
  }

  code {
    font-family: ui-monospace, "Cascadia Mono", "Consolas", monospace;
    font-size: 12.5px;
    background: var(--wash);
    border-radius: 5px;
    padding: 1px 5px;
  }
</style>
