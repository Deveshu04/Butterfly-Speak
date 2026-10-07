<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { onMount } from "svelte";
  import {
    cancelDownload,
    cloudDeleteAccount,
    cloudSignIn,
    cloudSignOut,
    cloudStatus,
    cloudUsage,
    deleteModel,
    downloadModel,
    ensureSupportModels,
    formatBytes,
    listModels,
    sarvamKeyStatus,
    selectModel,
    setSarvamKey,
    validateSarvamKey,
    SIGNED_OUT_HERE_ONLY,
    type CloudStatus,
    type CloudUsage,
    type ModelStatus,
    type Provider,
    type SarvamKeyStatus,
  } from "$lib/api";
  import Dropdown from "$lib/components/Dropdown.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import {
    CLOUD_AUTH_CHANGED,
    MODEL_PROGRESS,
    type ModelProgressPayload,
  } from "$lib/events";
  import { DASHBOARD_URL } from "$lib/sarvam";
  import { settings } from "$lib/stores.svelte";
  import "./rows.css";

  let s = $derived(settings.current);
  let models = $state<ModelStatus[]>([]);
  let progress = $state<Record<string, ModelProgressPayload>>({});
  let confirming = $state<string | null>(null);
  let error = $state("");

  let keyStatus = $state<SarvamKeyStatus | null>(null);
  let editingKey = $state(false);
  let keyInput = $state("");
  let keyError = $state("");
  let validating = $state(false);
  let removingKey = $state(false);
  /** The inline "Remove the saved key?" confirmation is open. */
  let confirmingRemove = $state(false);

  /** Editor is open while explicitly editing, or whenever no key is stored. */
  let keyOpen = $derived(editingKey || (keyStatus !== null && !keyStatus.present));

  let account = $state<CloudStatus | null>(null);
  let usage = $state<CloudUsage | null>(null);
  /** True between handing the sign-in to the browser and hearing back. The
   * round trip lands on `cloud-auth-changed` — success, failure or a declined
   * consent all end it. */
  let waitingForBrowser = $state(false);
  let cloudError = $state("");
  /** The quiet sentence after a sign-out Supabase never heard. Stays on the
   * signed-out card until the next sign-in starts. */
  let cloudNotice = $state("");
  /** The inline "Delete my Cloud account" confirmation is open. */
  let confirmingDelete = $state(false);
  let deleting = $state(false);
  let deleteError = $state("");

  /** Shown only while the count is unknown: before the relay has answered,
   * and whenever it can't be asked. Once it answers, its own `limit` is the
   * number printed. */
  const UNKNOWN_USAGE = "— / 2,000 words this week";
  /** Grouped the way the picker's "2,000 words a week" is, whatever the
   * machine's locale — the two sit on the same screen. */
  const count = (n: number) => n.toLocaleString("en-US");

  let usageLine = $derived(
    usage
      ? `${count(usage.words)} / ${count(usage.limit)} words this week`
      : UNKNOWN_USAGE,
  );

  /** The count is asked for only while Cloud is the engine and someone is
   * signed in. Every ask keeps the relay's copy of the account for another
   * week, which a user who has moved to another engine should not pay for by
   * opening Settings. */
  let showsUsage = $derived(s?.provider === "cloud" && account?.signedIn === true);

  $effect(() => {
    if (showsUsage) refreshUsage();
    else usage = null;
  });

  /** The sign-in is asked about only while Cloud is the engine. The first
   * ask after a launch renews it with Supabase, and a user who has moved to
   * another engine stays signed in on this computer without that contact. */
  let cloudSelected = $derived(s?.provider === "cloud");

  $effect(() => {
    if (cloudSelected) cloudStatus().then((st) => (account = st));
  });

  /** Punctuation support archive fetched alongside the ASR model. */
  const SUPPORT_BYTES = 30_600_000;
  /** Its download's id in progress events (`models::missing_support_jobs`). */
  const PUNCTUATION_ID = "punctuation";

  async function refresh() {
    models = await listModels();
  }

  onMount(() => {
    refresh();
    sarvamKeyStatus().then((k) => (keyStatus = k));
    const unsubs: Array<() => void> = [];
    listen<ModelProgressPayload>(MODEL_PROGRESS, (e) => {
      progress = { ...progress, [e.payload.id]: e.payload };
      if (["done", "error", "cancelled"].includes(e.payload.phase)) {
        refresh();
      }
    }).then((u) => unsubs.push(u));
    listen<CloudStatus>(CLOUD_AUTH_CHANGED, (e) => {
      account = e.payload;
      // The round trip has finished, whatever its result, so the card stops waiting.
      // The count follows `showsUsage`.
      waitingForBrowser = false;
      if (!e.payload.signedIn) closeDeleteConfirm();
    }).then((u) => unsubs.push(u));
    return () => unsubs.forEach((u) => u());
  });

  /** A count we can't get is a dash, never a zero and never a toast: offline
   * and "nothing dictated yet" are different facts, and neither is worth
   * interrupting someone for. */
  async function refreshUsage() {
    try {
      usage = await cloudUsage();
    } catch {
      usage = null;
    }
  }

  async function signIn() {
    cloudError = "";
    cloudNotice = "";
    waitingForBrowser = true;
    try {
      await cloudSignIn();
    } catch (e) {
      // Only ever "the browser wouldn't open" — the sign-in itself reports
      // through the event above.
      cloudError = String(e);
      waitingForBrowser = false;
    }
  }

  /** Sign out, and cancel a sign-in still out at the browser — the backend
   * treats both as the same thing, so the Cancel button and the Sign out
   * button are one call. It always signs out here; when Supabase couldn't be
   * reached, the card says what that leaves behind. */
  async function signOut() {
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

  /** On success the backend signs out and emits `cloud-auth-changed`, which
   * turns the card to its signed-out state. On failure the user is usually
   * still signed in and the sentence says whether to retry or sign in again;
   * if the sign-in turned out to be revoked, the backend has signed out, and
   * the sentence goes on the signed-out card instead. */
  async function deleteAccount() {
    deleteError = "";
    deleting = true;
    try {
      await cloudDeleteAccount();
      closeDeleteConfirm();
    } catch (e) {
      const message = String(e);
      const now = await cloudStatus();
      if (now.signedIn) {
        deleteError = message;
      } else {
        account = now;
        closeDeleteConfirm();
        cloudError = message;
      }
    } finally {
      deleting = false;
    }
  }

  function closeDeleteConfirm() {
    confirmingDelete = false;
    deleteError = "";
  }

  async function act(fn: () => Promise<unknown>) {
    error = "";
    try {
      await fn();
      await refresh();
      // Commands like select_model write settings backend-side; re-sync the
      // store so a later settings.update can't revert them from stale state.
      // Queued behind any save in flight, which would otherwise land after
      // it with the old snapshot; a failed re-read lands in `error` below.
      await settings.reloadChecked();
    } catch (e) {
      error = String(e);
    }
  }

  /** Bumped to force the provider select to re-render (native select state
   * diverges from settings when a save fails). */
  let providerEpoch = $state(0);
  /** Why the last engine change was not saved. Shown under the Engine row,
   * which every engine has. */
  let providerError = $state("");

  function setProvider(provider: Provider) {
    providerError = "";
    settings
      .update((st) => (st.provider = provider))
      .catch((e) => {
        providerError = String(e);
        providerEpoch += 1;
      });
  }

  async function saveKey() {
    keyError = "";
    validating = true;
    try {
      await validateSarvamKey(keyInput);
      keyInput = "";
      editingKey = false;
      keyStatus = await sarvamKeyStatus();
    } catch (e) {
      keyError = String(e);
    } finally {
      validating = false;
    }
  }

  function cancelKeyEdit() {
    editingKey = false;
    keyInput = "";
    keyError = "";
  }

  /** Deletes the key from the Windows credential store: an empty key is how
   * `set_sarvam_key` is told to remove it. Reached only through the inline
   * confirmation, which stays open with the reason if the removal fails. */
  async function removeKey() {
    keyError = "";
    removingKey = true;
    try {
      await setSarvamKey("");
      cancelKeyEdit();
      confirmingRemove = false;
      keyStatus = await sarvamKeyStatus();
    } catch (e) {
      keyError = String(e);
    } finally {
      removingKey = false;
    }
  }

  function onUse(m: ModelStatus) {
    if (m.verdict.kind === "notRecommended" && confirming !== m.id) {
      confirming = m.id;
      return;
    }
    confirming = null;
    act(() => selectModel(m.id));
  }

  /** Why the last download of a model failed, from its final progress event,
   * or null. The event stays in `progress` until the next attempt starts. */
  function failure(p: ModelProgressPayload | undefined): string | null {
    return p?.phase === "error" ? `Download failed: ${p.message ?? "unknown error"}` : null;
  }

  function startDownload(id: string) {
    // A new attempt starts without the last one's failure on screen, and so
    // does the punctuation model, which goes with every model download.
    progress = Object.fromEntries(
      Object.entries(progress).filter(([key]) => key !== id && key !== PUNCTUATION_ID),
    );
    act(async () => {
      ensureSupportModels();
      await downloadModel(id);
    });
  }

  function progressPct(p: ModelProgressPayload): number {
    if (p.total === 0) return 0;
    return Math.min(100, Math.round((p.downloaded / p.total) * 100));
  }
</script>

<h1 class="s-title">Speech engine</h1>

{#if s}
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Engine</p>
        <p class="s-row-sub">Where speech becomes text.</p>
        {#if providerError}
          <p class="s-error">{providerError}</p>
        {/if}
      </div>
      {#key providerEpoch}
        <Dropdown
          options={[
            {
              value: "cloud",
              label: "Cloud",
              sublabel:
                "Sign in with Google · 2,000 words a week, free while in beta",
            },
            {
              value: "sarvam",
              label: "Bring your own key",
              sublabel: "English + 22 Indian languages, live",
            },
            { value: "local", label: "On-device", sublabel: "On this machine · English" },
          ]}
          value={s.provider}
          onchange={(v) => setProvider(v as Provider)}
        />
      {/key}
    </div>
  </div>

  {#if s.provider === "cloud"}
    <p class="s-group">Butterfly Labs</p>
    <div class="s-panel">
      <div class="s-row account-row">
        <div class="account-head">
          <div class="s-info">
            <p class="s-row-title">Account</p>
            <p class="s-row-sub">
              {#if account?.signedIn}
                {account.email ?? "Signed in"}
              {:else if waitingForBrowser}
                Waiting for your browser…
              {:else if account === null}
                <!-- Not known yet: the first ask after switching to Cloud can
                     take a Supabase round trip, and a signed-in user must not
                     be shown the Sign in button meanwhile. -->
                Checking your sign-in…
              {:else}
                Sign-in opens in your browser.
              {/if}
            </p>
            {#if cloudNotice && !account?.signedIn}
              <p class="s-row-sub">{cloudNotice}</p>
            {/if}
            {#if cloudError}
              <p class="s-error">{cloudError}</p>
            {/if}
          </div>
          {#if account?.signedIn}
            <span class="s-value">{usageLine}</span>
            <button class="s-btn" disabled={deleting} onclick={signOut}>Sign out</button>
          {:else if account === null && !waitingForBrowser}
            <!-- Nothing to press until the sign-in is known. -->
          {:else}
            <button class="s-btn primary" disabled={waitingForBrowser} onclick={signIn}>
              Sign in with Google
            </button>
            {#if waitingForBrowser}
              <button class="s-btn" onclick={signOut}>Cancel</button>
            {/if}
          {/if}
        </div>
        {#if account?.signedIn}
          {#if confirmingDelete}
            <p class="s-row-sub delete-warning">
              This deletes your Butterfly Labs account. It can't be undone. If you sign
              in again with the same Google account before Monday 00:00 UTC, this
              week's usage carries over.
            </p>
            <div class="delete-actions">
              <button class="s-btn danger" disabled={deleting} onclick={deleteAccount}>
                {deleting ? "Deleting…" : "Delete"}
              </button>
              <button class="s-btn" disabled={deleting} onclick={closeDeleteConfirm}>
                Cancel
              </button>
            </div>
            {#if deleteError}
              <p class="s-error">{deleteError}</p>
            {/if}
          {:else}
            <button class="link delete-link" onclick={() => (confirmingDelete = true)}>
              Delete my Cloud account
            </button>
          {/if}
        {/if}
      </div>
    </div>
  {/if}

  {#if s.provider === "sarvam" || s.provider === "cloud"}
    <!-- Named for what the panel holds, not for whose servers hold it: on
         Cloud the key row is gone and the polish model is all that is left,
         and a Cloud user has no Sarvam account to recognise the heading by. -->
    <p class="s-group">{s.provider === "cloud" ? "Polish" : "Sarvam AI"}</p>
    <div class="s-panel">
      {#if s.provider === "sarvam"}
        <div class="s-row key-row">
          <div class="key-head">
            <div class="s-info">
              <p class="s-row-title">API key</p>
              <p class="s-row-sub">
                {#if keyStatus?.present}
                  {keyStatus.masked}
                {:else}
                  Not connected — new accounts include free credits.
                {/if}
              </p>
            </div>
            {#if !keyOpen && !confirmingRemove}
              <button class="s-btn" onclick={() => (editingKey = true)}>
                {keyStatus?.present ? "Change" : "Connect"}
              </button>
              {#if keyStatus?.present}
                <button class="s-btn" onclick={() => (confirmingRemove = true)}>Remove</button>
              {/if}
            {/if}
          </div>
          {#if confirmingRemove && !keyOpen}
            {@render removeConfirm()}
          {/if}
          {#if keyError && !keyOpen}
            <p class="s-error">{keyError}</p>
          {/if}
          {#if keyOpen}
            <div class="key-edit">
              <input
                class="s-input key-input"
                type="password"
                placeholder="Paste your Sarvam API key"
                bind:value={keyInput}
                onkeydown={(e) => {
                  if (e.key === "Enter" && keyInput.trim() && !validating) saveKey();
                  if (e.key === "Escape" && keyStatus?.present && !validating) {
                    e.stopPropagation();
                    cancelKeyEdit();
                  }
                }}
              />
              <button
                class="s-btn primary"
                disabled={!keyInput.trim() || validating}
                onclick={saveKey}
              >
                {validating ? "Checking…" : "Validate & save"}
              </button>
              {#if keyStatus?.present}
                <button class="s-btn" disabled={validating} onclick={cancelKeyEdit}>
                  Cancel
                </button>
              {/if}
            </div>
            {#if keyError}
              <p class="s-error">{keyError}</p>
            {/if}
            <button class="link" onclick={() => openUrl(DASHBOARD_URL)}>
              Get a key ↗
            </button>
          {/if}
        </div>
      {/if}

      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title">Polish model</p>
          <p class="s-row-sub">The brain behind AI Polish and Transforms.</p>
        </div>
        <Dropdown
          options={[
            { value: "sarvam-105b", label: "Sarvam 105B", sublabel: "Recommended" },
            {
              value: "sarvam-105b-conversations",
              label: "Sarvam 105B Conversations",
              sublabel: "Longer context, noticeably slower",
            },
          ]}
          value={s.sarvam.polishModel}
          onchange={(v) => settings.update((st) => (st.sarvam.polishModel = v))}
        />
      </div>
    </div>
  {:else if s.provider === "local"}
    <p class="s-group">On-device models</p>
    {@const sel = models.find((m) => m.selected)}
    {#if sel && !sel.installed}
      {@const p = progress[sel.id]}
      <div class="s-panel">
        <div class="s-row">
          <div class="s-info">
            <p class="s-row-title">Download required models</p>
            <p class="s-row-sub">
              {sel.displayName} + support files · about
              {formatBytes(sel.diskBytes + SUPPORT_BYTES)}
            </p>
            {#if failure(p)}
              <p class="s-error">{failure(p)}</p>
            {/if}
          </div>
          {#if sel.downloading && p && p.phase === "downloading"}
            <div class="prog">
              <div class="bar" style="width: {progressPct(p)}%"></div>
            </div>
            <span class="s-value">{progressPct(p)}%</span>
          {:else if sel.downloading}
            <span class="s-value">{p?.phase ?? "starting"}…</span>
          {:else}
            <button class="s-btn primary" onclick={() => startDownload(sel.id)}>
              Download
            </button>
          {/if}
        </div>
      </div>
    {/if}

    <div class="s-panel">
      {#each models as m (m.id)}
        {@const p = progress[m.id]}
        <div class="s-row">
          <div class="s-info">
            <p class="s-row-title">
              {m.displayName}
              {#if m.selected && m.installed && s.provider === "local"}
                <!-- Genuinely running: this exact combination is when the
                     backend loads the model into the ASR thread. -->
                <span class="active-chip">Active</span>
              {:else if m.selected}
                <span class="default-chip">Default</span>
              {/if}
            </p>
            <p class="s-row-sub">
              {formatBytes(m.diskBytes)} · ~{formatBytes(m.estRamBytes)} RAM ·
              ~{m.werPct}% WER
            </p>
            <!-- The selected model's failure is already under the Download
                 required models row above. -->
            {#if failure(p) && m.id !== sel?.id}
              <p class="s-error">{failure(p)}</p>
            {/if}
          </div>
          {#if m.downloading && p && p.phase === "downloading"}
            <div class="prog">
              <div class="bar" style="width: {progressPct(p)}%"></div>
            </div>
            <button class="s-btn" onclick={() => act(() => cancelDownload(m.id))}>
              Cancel
            </button>
          {:else if m.downloading}
            <span class="s-value">{p?.phase ?? "starting"}…</span>
          {:else if !m.installed}
            <button class="s-btn" onclick={() => startDownload(m.id)}>Download</button>
          {:else if !m.selected}
            {#if confirming === m.id}
              <button class="s-btn danger" onclick={() => onUse(m)}>Use anyway</button>
              <button class="s-btn" onclick={() => (confirming = null)}>
                Never mind
              </button>
            {:else}
              <button class="s-btn" onclick={() => onUse(m)}>Use</button>
              <button
                class="icon-btn"
                aria-label="Delete {m.displayName}"
                onclick={() => act(() => deleteModel(m.id))}
              >
                <Icon name="trash" size={16} stroke={1.7} />
              </button>
            {/if}
          {/if}
        </div>
      {/each}
    </div>
    {#if failure(progress[PUNCTUATION_ID])}
      <p class="s-error">
        The punctuation model goes with every speech model. {failure(progress[PUNCTUATION_ID])}
      </p>
    {/if}
    {#if error}
      <p class="s-error">{error}</p>
    {/if}
  {/if}

  <!-- A key saved under Bring your own key stays in use after a switch to
       another engine, so it can be removed from here too. -->
  {#if s.provider !== "sarvam" && keyStatus?.present}
    <p class="s-group">Sarvam AI</p>
    <div class="s-panel">
      <div class="s-row key-row">
        <div class="key-head">
          <div class="s-info">
            <p class="s-row-title">Saved API key</p>
            <p class="s-row-sub">
              {keyStatus.masked} · Importing a recording and the translate shortcut use it
              with any engine{s.provider === "local"
                ? "; with On-device, so do transforms, the voice agent, note actions, Auto-title and the prompt tester, unless your own AI endpoint is on for AI Polish"
                : ""}.
            </p>
          </div>
          {#if !confirmingRemove}
            <button class="s-btn" onclick={() => (confirmingRemove = true)}>Remove</button>
          {/if}
        </div>
        {#if confirmingRemove}
          {@render removeConfirm()}
        {/if}
        {#if keyError}
          <p class="s-error">{keyError}</p>
        {/if}
      </div>
    </div>
  {/if}
{/if}

<!-- Asked inline before the key goes, as Delete my Cloud account is: the key
     cannot be read back, so removing it by mistake means finding it again in
     the Sarvam dashboard. -->
{#snippet removeConfirm()}
  <p class="s-row-sub delete-warning">
    Remove the saved key? It is deleted from the Windows credential store, and you'll need to
    paste it again to use it.
  </p>
  <div class="delete-actions">
    <button class="s-btn danger" disabled={removingKey} onclick={removeKey}>
      {removingKey ? "Removing…" : "Remove"}
    </button>
    <button
      class="s-btn"
      disabled={removingKey}
      onclick={() => {
        confirmingRemove = false;
        keyError = "";
      }}
    >
      Cancel
    </button>
  </div>
{/snippet}

<style>
  /* API key row expands below the title/sub instead of holding one control;
     so does the account row, for its delete confirmation. */
  .key-row,
  .account-row {
    flex-direction: column;
    align-items: stretch;
    gap: 0;
  }

  .key-head,
  .account-head {
    display: flex;
    align-items: center;
    gap: 24px;
  }

  .key-edit,
  .delete-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 12px;
  }

  /* Under Sign out, not under the address. */
  button.link.delete-link {
    align-self: flex-end;
  }

  .delete-warning {
    margin-top: 12px;
  }

  .key-input {
    flex: 1;
    max-width: none;
  }

  button.link {
    align-self: flex-start;
    background: none;
    border: none;
    padding: 0;
    margin-top: 10px;
    font-family: var(--font-ui);
    font-size: 12.5px;
    color: var(--fg-muted);
    text-decoration: underline;
    cursor: pointer;
  }

  .active-chip {
    display: inline-block;
    vertical-align: 1px;
    margin-left: 8px;
    padding: 1px 9px;
    border-radius: 999px;
    background: var(--teal-soft);
    color: var(--teal);
    font-size: 11.5px;
    font-weight: 650;
  }

  /* Selected as the on-device choice, but not currently running (cloud
     provider active, or the model isn't downloaded). */
  .default-chip {
    display: inline-block;
    vertical-align: 1px;
    margin-left: 8px;
    padding: 1px 9px;
    border-radius: 999px;
    background: var(--wash);
    color: var(--fg-muted);
    font-size: 11.5px;
    font-weight: 650;
  }

  .prog {
    flex: none;
    width: 160px;
    height: 6px;
    border-radius: 999px;
    background: var(--surface);
    border: 1px solid var(--hairline);
    overflow: hidden;
  }

  .bar {
    height: 100%;
    background: var(--accent);
    transition: width 200ms linear;
  }

  .s-btn.danger {
    background: var(--danger);
    border-color: var(--danger);
    color: var(--danger-fg);
  }

  .icon-btn {
    flex: none;
    display: grid;
    place-items: center;
    width: 34px;
    height: 34px;
    border: none;
    border-radius: 10px;
    background: transparent;
    color: var(--fg-muted);
    cursor: pointer;
    transition: background var(--motion);
  }

  .icon-btn:hover {
    background: var(--wash);
    color: var(--danger);
  }
</style>
