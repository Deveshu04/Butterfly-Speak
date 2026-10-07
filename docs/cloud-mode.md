# Cloud mode

Cloud mode lets someone dictate without a Sarvam account. They sign in with Google, and the app
sends their audio and text to the Butterfly Labs relay, a Cloudflare Worker in `relay/`, which
forwards them to Sarvam AI under the Butterfly Labs key. That key exists only as a secret of the
deployed Worker; the app holds none. [`relay/README.md`](../relay/README.md) documents every
route, close code and rule.

## What goes through the relay

| Route | Used for |
|---|---|
| `GET /v1/realtime` (WebSocket) | Dictation: audio up, Sarvam's transcripts down |
| `POST /v1/chat/completions` | AI Polish, transforms, the voice agent, note actions, Auto-title and the prompt tester |
| `GET /v1/usage` | The weekly word count shown in Settings |
| `DELETE /v1/account` | **Delete my Cloud account** |

Every request carries the Supabase access token as `Authorization: Bearer …`. The Worker checks
it against the Supabase project's public signing keys and accepts only accounts that signed in
with Google. It then hands the request to that user's Durable Object, which holds the weekly
counter and dials Sarvam.

Recording import and the translate shortcut do not go through the relay. They need a saved
Sarvam key, and with one they go straight to Sarvam. The one-shot batch retry that Bring your own
key makes for a failed realtime session does not exist in Cloud mode.

## What the relay sees

- **Audio**, measured by the length of each frame's base64 payload, never decoded or kept.
- **Your Dictionary words**, sent as spelling hints in the realtime URL. Workers Logs are off in
  `relay/wrangler.toml`, because they would record that URL.
- **Transcripts.** Sarvam's frames reach the app unchanged. The relay counts the words of each
  final transcript and discards the text in the same step.
- **Chat requests and replies**, checked for shape on the way in and streamed back untouched.
- **Your e-mail address**, inside the sign-in token. The Worker uses it only to compute an
  HMAC-SHA256 key so that deleting the account cannot reset the week; the Durable Object receives
  the key, never the address.

Only allow-listed events, query parameters and chat fields reach Sarvam; anything else is
dropped or refused. In normal operation the relay logs nothing. On failure it logs a fixed
message, at most with an HTTP status or an error name.

## The limits

| Limit | Value | What happens |
|---|---|---|
| Words a week | 2,000 | New sessions are refused with close code `4029`, reason `quota` |
| Grace | 100 words | A running session may go on to 2,100 words, then every open session closes with `4029` |
| Audio a week | 2 hours, silence included | Close `4029` |
| Chat calls a week | 3,000 | `429` |
| Session length | 30 minutes | Close `4030`, reason `session_limit` |
| Idle session | Audio a minute behind the clock, or none 10 seconds after opening | Close `1008`, reason `idle` |
| Accounts | Google sign-ins only | `401` |

The values are the `[vars]` in `relay/wrangler.toml`; the per-minute rate limits are in
`relay/README.md`. The week starts on Monday at 00:00 UTC, when words, audio and chat calls all
reset. Words are counted from Sarvam's own transcripts, split on whitespace, so every script
counts the same way.

## What is stored

**Supabase** (Mumbai region) holds what Google provides at sign-in (e-mail address, name,
profile picture link, Google account id), the Supabase user id, the sign-in sessions, and one
`usage_weekly` row per week dictated. That row is a copy of the relay's word count, written at
most every ten seconds. The limit is enforced in the Durable Object, so editing the table changes
nothing; the app reads usage from `GET /v1/usage`. A scheduled job deletes unfinished sign-ins
once they are a day old.

**The user's Durable Object** holds the user id and the counter (week, words, chat calls,
milliseconds of audio) for the current week, and the previous week's until its word count has
reached Supabase. It deletes all of it by itself within about a week after the app last contacts
the relay for that account. It lives in a Cloudflare data centre near where the user's connection
first reaches Cloudflare, which may be outside India.

**After an account deletion**, the user's object keeps only a two-hour note that the account is
deleted, so that it refuses the account's earlier tokens. A `Carry` object keeps that week's
counts under the HMAC of the e-mail address until the week ends.

No transcript, audio, prompt or token is stored anywhere in the relay.

## Overhead

Measured on 24 September 2026 from a connection in India that enters Cloudflare at Marseille,
the relay added about 0.6 s between the end of speech and the polished text, compared with
Bring your own key. See
[benchmarks.md](benchmarks.md#cloud-mode).

## Working on the relay

`relay/README.md` covers running it locally, proving the weekly limit with a lower
`WEEKLY_WORD_LIMIT`, and the tests.

The app talks to `https://butterflylabs-relay.butterflylabs.workers.dev` (`DEFAULT_RELAY_URL` in
`src-tauri/src/sarvam/mod.rs`). To point a development build at a local relay, add
`"cloud": { "relayUrl": "http://127.0.0.1:8787" }` to `%APPDATA%\ButterflySpeak\settings.json`.
The override is accepted only for `https` addresses and plain `http` to this machine; anything
else falls back to the default. The app always signs in with the Butterfly Labs Supabase project
(`src-tauri/src/auth/session.rs`), so a relay you run must verify tokens from that project.
