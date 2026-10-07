# Application flow

How a dictation, a Cloud sign-in and an update check run, step by step. Module names are under
`src-tauri/src`.

## A dictation

### 1. Chord down

The keyboard hook (`hotkeys.rs`) sees the dictation chord, `Ctrl+Win` by default, and sends
`ChordDown` to the controller. The chord sets the dictation's intent once, at this moment: the
main chord types the text, the translate chord translates it, the voice-agent chord sends it to
the agent (`routes/`). The controller also records which window has focus (`foreground.rs`), so
the text can be pasted there later even if the user switches windows.

Holding the chord is push-to-talk. Releasing it within 350 ms counts as a tap; two taps start
hands-free mode, and one more tap finishes it. Esc cancels a recording. A push-to-talk hold
stops by itself after five minutes.

At chord-down the controller also picks the transcriber for this dictation (`SttPath`):

| Lane | When | How it hears the audio |
|---|---|---|
| Sarvam realtime | Cloud or Bring your own key | Streamed over a WebSocket while the user speaks (`sarvam/ws.rs`) |
| On-device | On-device engine | Recorded whole, then decoded by a sherpa-onnx model (`asr/offline.rs`) |
| Custom endpoint | **Use for speech-to-text** is on | Recorded whole, then posted once to `/audio/transcriptions` (`asr/custom.rs`) |

For the Sarvam lane, `Lane` then picks the host: Sarvam directly with the user's key, or the
relay with the Cloud sign-in token. The dispatcher opens one realtime session per dictation.

### 2. Capture

`audio.rs` reads the microphone, resamples to 16 kHz mono and sends a chunk every 100 ms. It
keeps 300 ms of audio from before the press, so the first syllable is not cut off. The overlay
pill shows a waveform and the listening state, not a running transcript.

On the Sarvam lane every chunk is encoded as base64 PCM16 and sent as it arrives; chunks
captured while the socket is still connecting are buffered and sent once the session begins.
Push-to-talk uses manual endpointing (the key release ends the speech); hands-free uses Sarvam's
voice activity detection, which splits the speech at pauses.

### 3. Partials and finals

Sarvam answers with partial transcripts and, per utterance, a final one. Partials stay inside
the dispatcher and are never shown. Finals are kept per utterance index, so a later final for the
same utterance replaces the earlier one. If the last utterance's final never arrives, its latest
partial is appended in its place (`assemble` in `sarvam/ws.rs`).

### 4. Polishing while the user speaks

When AI Polish is on, the Sarvam lane polishes closed sentences before the user has finished
(`sarvam/incremental.rs`). As finals arrive, a segmenter hands out a chunk once at least 50 words
end in a sentence mark; a run-on with no sentence end is cut after 120 words at the last comma or
space. One worker per dictation polishes the chunks in order. Each call carries up to 600
characters of the already polished text as context, and each result goes through the guardrail
and a deterministic seam repair that removes a repeat of the text before it and capitalises a
new sentence.

### 5. Release

On release the controller waits up to 250 ms for the last audio, then drops the recording if no
part of it was audible (`speech_gate.rs`) and shows "Didn't catch that".

On the Sarvam lane the dispatcher sends the finish frames (`speech_end` and `end` for
push-to-talk, `end` for hands-free) and waits for the remaining finals. It stops as soon as Sarvam
sends `session.end`; otherwise it waits at most 4 s plus 1 ms per 30 ms of speech, capped at
6 s. If a Bring-your-own-key session dies with no usable transcript and the recording is between
0.5 and 30 seconds long, the same audio gets one attempt at Sarvam's batch endpoint. A
transcript that is still incomplete is saved to History and reported, not pasted.

### 6. Clean-up and the tail polish

The transcript goes through the rule pipeline (`cleanup/`): spoken "new line" and "new
paragraph", the user's corrections and snippets, and on-device also fillers, self-corrections,
numbers and dates and the punctuation model.

Then AI Polish runs on what was not polished yet. On the Sarvam lane that is only the tail after
the last chunk, sent with the chunks' polished text as context, and the result is joined to the
chunks. A dictation too short to produce a chunk is polished in one call. A call has 6 s; a
call that fails, times out or is rejected by the guardrail leaves the rule-cleaned text in place
and shows a notice. On-device, Qwen2.5-0.5B polishes inputs of up to 120 words. The cleanup level
(Settings → Cleanup) sets how much the model may change.

### 7. The route

`routes/` turns the text into what gets typed: the text itself, a translation from Sarvam's
`/translate` (falling back to the untranslated text with a notice), or the voice agent's answer.
The agent never types the transcript in place of an answer.

### 8. Paste and restore

`injection.rs` saves the clipboard's text, puts the dictation on the clipboard marked so that
Windows clipboard history and cloud sync skip it, brings the target window back and sends
`Ctrl+V`, or `Ctrl+Shift+V` in a terminal. It then puts the saved text back, unless another
program has written to the clipboard in the meantime. The dictation is added to History, and
`learn/` starts watching the field for words the user retypes.

## Signing in to Cloud mode

1. **Settings → Speech engine → Cloud → Sign in with Google** calls `cloud_sign_in`
   (`auth/commands.rs`). The app makes a PKCE verifier, keeps it in memory, and opens the system
   browser at Supabase's `/auth/v1/authorize?provider=google` with the S256 challenge.
2. The user signs in on Google's own page. Supabase redirects the browser to
   `butterflylabs://auth/callback?code=…`. The installer registers that scheme; Windows starts the
   app with the URL, and the single-instance plugin hands it to the running copy.
3. The app accepts only a callback carrying a code, and exchanges it with the verifier at
   `/auth/v1/token?grant_type=pkce`.
4. The access token stays in memory and is refreshed when it has less than five minutes left.
   The refresh token is stored in Windows Credential Manager. Supabase rotates refresh tokens on
   every use, so refreshes run one at a time. Neither token reaches the webview, `settings.json`
   or the logs.
5. Each relay request carries the access token as `Authorization: Bearer …`. **Sign out** calls
   `/auth/v1/logout?scope=local` and deletes the stored token.

## Checking for updates

`updater.rs` checks the feed at
`https://github.com/Deveshu04/Butterfly-Speak/releases/latest/download/latest.json` 30 seconds
after launch and every eight hours while the app runs. The launch check is skipped if the last
check was less than an hour ago. **Settings → System → Check for updates automatically** turns
the automatic checks off, and then the app sends no request; **Check for updates** on the
**Help & about** page always asks.

A newer version is shown on the **Help & about** page with its release notes as plain text. Nothing is
downloaded until the user presses **Install and restart**, which is refused while a dictation or
an import is running. The download is verified against the public key in
`src-tauri/tauri.conf.json` before the installer runs; the installer then runs in passive mode
and the app restarts.
