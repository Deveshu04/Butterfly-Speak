# Butterfly Labs relay

The Cloudflare Worker behind **Cloud mode** in Butterfly Speak. It is the only
piece of the product that holds a Sarvam key: the app signs in with Google
through Supabase and talks to this relay with its Supabase access token, and the
relay talks to Sarvam with Butterfly Labs' key. Cloud mode gives a signed-in user
**2,000 dictated words a week, free while in beta**.

It is published here because the app is open source and you should be able to
read exactly what the server in the middle does — and see that it stores nothing
of what you say.

## What it does

A request arrives with `Authorization: Bearer <supabase access token>` — the
header only; a token in the query string is not read, because a bearer in a URL
ends up in logs and histories. The Worker verifies that token against the
project's **public** JWKS (`/auth/v1/.well-known/jwks.json`, keys cached ten
minutes), checks `exp`, the `authenticated` audience and role, and takes the
user id from `sub`. Nothing in the URL or the body ever names a user.

**Only Google identities get Cloud quota.** The token's `app_metadata.provider`
must be `google` and `is_anonymous` must not be `true`; anything else is `401`.
With the Email provider switched on, anyone holding the public anon key could
sign up email identities at will, each with its own weekly allowance. The
project keeps that provider off, and this rule is the defence in depth behind
it. `app_metadata` is written only by Supabase Auth; `user_metadata` is the
user's to edit and is never consulted. An account first created by email and
later linked to Google keeps `provider: "email"`, and is refused.

The request is then handed to that user's
Durable Object (`idFromName("user:" + sub)`), which is the only place the weekly
counter lives. The object has no location hint: Cloudflare creates it next to
wherever that user first reaches Cloudflare, so the Worker-to-object hop stays
short whichever edge a user's network lands on. That data centre may be
outside India.

| route | what happens |
|---|---|
| `GET /v1/realtime` (WebSocket) | The object refuses with close code **`4029`**, reason `quota`, when the week's words or its audio are already spent. Otherwise it opens the upstream socket to `wss://api.sarvam.ai/speech-to-text-realtime/ws` with the `api-subscription-key` header. Sarvam's frames reach the app **unchanged**. The app's frames are allow-listed: only the events the app's own `ClientMsg` sends (`audio_input` with a base64 `audio` string, `speech_start`, `speech_end`, `end`, and the `ping` keepalive; not `flush`, which the app does not send) pass, each rebuilt in exactly the form the app writes it, with no other key; anything else, binary frames included, is dropped. The query is rebuilt from an allow-list too (`language_code`, `stream_type`, `mode`, `endpointing`, `prompt`; `model`, `encoding`, `sample_rate` pinned), each value percent-encoded as the app encodes its own (`%20` for a space, never `+`). Nothing client-supplied reaches Sarvam verbatim, neither in the URL nor in a frame. At most five sockets per user at once, `429` past that; the slot is taken before the upstream is dialled, so simultaneous upgrades cannot all slip through. At most 20 sessions opened per user a minute, `429` with the body `rate limited` past that, before anything else is done. An open session is closed with `4029`/`quota` when the week's words reach the limit plus 100 or its audio reaches two hours, with **`4030`**, reason `session_limit`, after 30 minutes, with **`1008`**, reason `idle`, once its audio falls a minute behind the clock or when it has sent no audio ten seconds after it opened, and with `1008`, reason `rate`, when it floods frames (see *The limits inside a session*). |
| `POST /v1/chat/completions` | The polish/transform/agent/notes call, forwarded with the key, request and response streamed through untouched (SSE included). Shaped first: JSON object ≤ 128 KiB *of UTF-8*, read only that far even when it is sent without a length (so an Indic body is measured the same way as an English one; a long note in Devanagari can pass 64 KB), `model` one of the app's two Sarvam ids, `max_tokens` ≤ 8192, and only the keys `model`, `messages`, `temperature`, `max_tokens`, `stream`, `stream_options`, `reasoning_effort`. 60 calls a minute per user, and **3,000 a week** (`WEEKLY_CHAT_LIMIT`), `429` with the body `weekly chat limit` past it. The app makes about one call per dictation plus one per ~50 words, so honest use never meets the weekly cap; it is there so the route cannot be used as a free LLM. This is a formatter's proxy, not a general LLM proxy. |
| `GET /v1/usage` | `{ "week_start": "YYYY-MM-DD", "words": n, "limit": 2000 }` for the Settings card. 60 reads a minute per user, `429` past that. |
| `DELETE /v1/account` | **Delete my Cloud account** in the app. The object closes every open session of the user with `1000`, reason `account deleted` (an upgrade still waiting on Sarvam included), hands this week's counts, the audio and the partials' words those sessions still held included, to the carry record for the user's address (see *A deletion does not reset the week*; if that fails, nothing is deleted and the answer is `502`), cancels its flush alarm and deletes all of its storage. Then the relay deletes the user from Supabase Auth with GoTrue's admin API (`DELETE /auth/v1/admin/users/<sub>`, with the service role), and the user's `usage_weekly` rows go with it (`on delete cascade`). GoTrue's `404` with `error_code` `user_not_found` means the user is already gone; any other `404` is a failure. Once the user is gone, the object carries over whatever the account's token spent while GoTrue answered, erases everything again and keeps only a tombstone, `deleted_until`, two hours on, with an alarm that erases it; the answer is `204`. While the tombstone is there, every other route answers `401` with the body `account deleted` and stores nothing, because the account's access tokens stay valid for up to an hour, and a repeated delete answers `204` without asking GoTrue again. `502` when Supabase fails: the object is already empty, no tombstone is written because the account still exists, and calling again finishes the job. One delete runs at a time: a second one that arrives while it runs waits for it and gets the same answer. 10 calls a minute per user, `429` past that. |

Anything else is `404`; a missing or bad token is `401`. A token signed with a
key that is not in the cached set makes the relay fetch the set again, but not
within 30 seconds of its last fetch; until then, and if the key is still not
in the set, the answer is `401`. So a new signing key should be in the
published set for ten minutes, the cache's life, before tokens are signed with
it. If the key set itself cannot be reached — a Supabase outage — the answer
is `503` with `retry-after: 5`, so the app retries instead of treating a good
session as dead.

## The counter

The week is the **ISO week**, Monday 00:00 UTC. Words are counted only from
Sarvam's own frames, whitespace-separated so it counts the same way in every
script: every `transcript.final`, and, when a session ends, however it ends,
every utterance that has had `transcript.partial` frames but no final, for the
most words any of those partials held. A modified client cannot lower the count
by hanging up before a final, or by deleting its account first, since the
partials it was sent are counted then and carried with the week; it can only
stop sending audio. Each utterance is counted once: the app keeps one final
per `utterance_idx`, a later one replacing the earlier, so a second final for
the same utterance, or its partials counted at the end, add only the words
they have beyond the most any earlier final held. The same counter holds the
week's chat calls and its audio time, and all three start again at zero on
Monday.

The Durable Object's storage is the counter of record. No client can address or
read it, and the one route that deletes it, `DELETE /v1/account`, carries the
week over first. Every ten seconds at most, the object mirrors
`{ user_id, week_start, words, updated_at }` into Supabase `public.usage_weekly`
through PostgREST with the service role, so the app (and later the site) can
display the number. Only words are mirrored. That mirror is for display only:
editing or deleting a row there changes nothing about enforcement.

## The limits inside a session

A session that starts under the weekly limit is not cut the moment the limit
is crossed: the utterance in flight is delivered, and the session may run on
**at most 100 words past 2,000** (`WORD_GRACE`). When a final takes the week to
2,100, that final is sent to the app first and then every open session of the
user is closed with `4029`/`quota`, the Sarvam side closed normally. A new
session is refused as soon as the week reaches 2,000.

A session lasts **at most 30 minutes** (`SESSION_MAX_SECONDS`, 1800): after
that, whatever it is doing, the app's socket is closed with `4030`, reason
`session_limit`, and Sarvam's is closed with it.

A session that has stopped streaming audio is closed as **idle**, with `1008`,
reason `idle`, on both sides. The app streams every 100 ms of microphone audio,
silence included, for as long as a dictation records, in push-to-talk and
hands-free alike, and after its last audio waits at most six seconds for the
final transcript. So the rule is measured in audio time: a session whose audio
falls **a minute behind the clock** (`SESSION_IDLE_SECONDS`, 60) is not the
app. Nor is one that has sent **no audio at all ten seconds** after it opened:
the app sends the audio it buffered while connecting as soon as Sarvam has
begun the session. Pings, or a trickle of tiny frames, keep nothing open. A
socket that sends no audio spends no words and no audio, yet keeps Durable
Object time running and one of Sarvam's concurrent connections taken. These
rules, and the 20 sessions an account may open a minute, bound how long it can
do that: a socket that sends nothing lasts ten seconds, and one that sends a
little audio and then stops, a minute.

A session that **floods frames** is closed with `1008`, reason `rate`: more than
50 frames in any one second, counting every frame, forwarded or dropped, except
full audio chunks. Every inbound frame costs the relay part of a Durable Object
request, forwarded or not. The app sends one chunk per 100 ms of audio and a
handful of control frames per dictation. Its chunks are left out of the count
because it also sends every chunk it buffered while connecting in one burst
when the session begins, a hundred at once after a retried connect; a flood of
real chunks is bounded by the audio budget instead.

A session whose upgrade was still on its way to Sarvam when the week was cut is
closed with `4029`/`quota` as soon as Sarvam answers, like the ones that were
already open.

Each account may stream **two hours of audio a week** to Sarvam
(`WEEKLY_AUDIO_SECONDS`, 7200). Silence produces no words for the word limit
to catch, yet every second of it is still audio sent to Sarvam on Butterfly
Labs' key; the audio budget is what bounds a session that only sends silence. The relay measures
each `audio_input` frame by the length of its base64 payload (16 kHz mono PCM16,
the format it pins upstream: 32,000 bytes a second), never decoding or keeping
it. The time is held in memory per session and written to the weekly counter
after every ten seconds of audio and when the session closes, rather than once
a frame. A new session is refused with `4029`/`quota` once the week's audio has
reached the budget, and every open session is closed with `4029`/`quota` the
moment the stored and the not-yet-written audio together reach it. Audio time
is not mirrored to Supabase, and `GET /v1/usage` does not report it.

## A deletion does not reset the week

Signing in again after `DELETE /v1/account` makes a new Supabase user with a
new id, and so a new object with a fresh counter. So the week is carried over
by address:

* The Worker computes `HMAC-SHA256(CARRY_KEY, lower(trim(email)))` from the
  verified token's `email` claim and passes only that hex key to the user's
  object, in `x-carry-key` (one sent by the client is dropped). The object
  never sees the address.
* On a delete, before its storage goes, the object gives this ISO week's
  words, chat calls and audio to the `Carry` object named `carry:<key>`. A
  second deletion in the same week keeps the larger of each count. That object
  holds the one record and nothing else, and an alarm deletes it at the next
  Monday 00:00 UTC.
* A user object that has no counter yet asks `carry:<key>` when it makes one,
  and starts from those counts if they are for the current week. The limits
  then apply as if the account had never been deleted.
* The deleted account's own tokens cannot spend anything past the carry: its
  object keeps the tombstone described under `DELETE /v1/account` and refuses
  them. The tombstone is written the moment GoTrue confirms the deletion, and
  every read and spend of the week checks it inside the counter queue, so a
  request that got in just before it (an upgrade on its way to Sarvam, a chat
  call whose body is still arriving) is refused once it reaches that queue.
  One already inside the queue when the tombstone lands may still spend; the
  carry that follows takes it in, and the erase removes its counter before
  the delete answers. What the tombstone refuses is added to that carry too:
  the audio and words the sessions it closes had been sent, and any count
  still queued when it went in.

`CARRY_KEY` is 32 random bytes written as 64 hex characters
(`openssl rand -hex 32`), and the relay uses that 64-character string itself
as the HMAC key. Without it, deleting and signing in work as before, nothing
is carried, and the Worker logs `carry disabled`.

## An unused object erases itself

Every object that holds anything keeps an alarm armed by the next Monday
00:00 UTC. It is armed when the user id is written, whatever the request goes
on to do, and whenever the counter is read; a flush brings it forward, and
then arms it for Monday again. When it fires after the counter's week has
ended, with no parked week waiting for Supabase and no session open or on its
way to Sarvam, the object deletes all of its storage; an alarm that finds a
tombstone keeps it instead, whenever the delete happened. So a user's copy is gone
within about a week after the app last contacts the relay for that account,
whether the account was deleted from Supabase by hand or simply left alone. A
week Supabase did not take is tried again within the hour; a row Supabase
refuses because its user no longer exists (a foreign-key `409`) can never
land, and is let go. A user who comes back starts from zero, or from a week
carried over a deletion. An object written by a version of the relay older
than this rule gets its alarm on its next request, and not before.

## Nothing you say is stored

The object's storage holds `counter` (the week, its words, chat calls and
milliseconds of audio),
`counter_prev` (last week's counter, only across a week rollover and only until
it has reached the mirror) and `user_id`. No transcript, no audio, no prompt, no
token. `DELETE /v1/account` deletes all three and leaves only `deleted_until`
for two hours, and the object deletes them itself about a week after its last
use. A `Carry` object holds, for a deleted address, one week's counts under
the address's keyed hash, until that week ends. Cloudflare may keep a recovery
history of Durable Object storage for up to 30 days. The inspections of the stream are these: a final's or a partial's words
are counted and its text discarded in the same expression (a partial's count
is held in memory until its final comes or the session ends), an app frame's event name is
checked against the allow-list, and audio is measured by the length of its
payload without being decoded. The relay logs nothing in normal operation
except one `carry disabled` line per isolate when `CARRY_KEY` is not set, and
on failure logs only a status code or a count — never a frame, a token, a key
or an address. `wrangler.toml` also turns Workers Logs off: they would record
each request's URL, and the realtime request's URL carries the user's
Dictionary words as `prompt`. `wrangler tail` still streams live logs to
whoever runs it, for debugging.

## Deploying it (Butterfly Labs only)

```sh
npm install
npx wrangler secret put SARVAM_API_KEY              # paste the Sarvam key
npx wrangler secret put SUPABASE_SERVICE_ROLE_KEY   # Supabase -> Settings -> API
openssl rand -hex 32 | npx wrangler secret put CARRY_KEY
npx wrangler deploy
```

The account needs a workers.dev subdomain before the first deploy, and Wrangler 4
no longer registers one itself. Opening Workers & Pages in the Cloudflare
dashboard creates one named after the account; to choose the name instead, send
`PUT /client/v4/accounts/<account id>/workers/subdomain` with
`{"subdomain": "<name>"}`. A new subdomain's certificate takes a few minutes to
issue, and TLS handshakes fail until it has.

`wrangler deploy` prints the URL. Butterfly Labs' relay is
`https://butterflylabs-relay.butterflylabs.workers.dev`, which is what the app is
built against (`DEFAULT_RELAY_URL`). Verify with `curl -i <url>/v1/usage`, which
must answer `401` without a token. `wrangler.toml` turns per-version preview URLs
off, so only the deployed version is ever reachable.

The three secrets live in Wrangler and nowhere else: not in `wrangler.toml`, not
in this repository, not in the app. `wrangler.toml` also carries no account id.
Records carried before a change of `CARRY_KEY` are not found after it; they
are still erased when their week ends.

## Running it locally

Put the secrets in `relay/.dev.vars` (git-ignored, never committed):

```
SARVAM_API_KEY = "..."
SUPABASE_SERVICE_ROLE_KEY = "..."
CARRY_KEY = "..."
```

then `npm run dev`. Point the app's relay URL at the printed localhost address.

## Proving the quota locally

The weekly limit is the one behaviour that cannot be tried out against the
deployed Worker: 2,000 words is a week of dictation, and spending them leaves
the account refused until Monday. `WEEKLY_WORD_LIMIT` is an ordinary var
(`user_session.ts` reads it as `Number(env.WEEKLY_WORD_LIMIT)` with a
fallback), and `wrangler dev --var` overrides what `wrangler.toml` says, so a
local run can reach the limit in three dictations instead of a week:

```sh
cd relay
npx wrangler dev --var WEEKLY_WORD_LIMIT:50     # .dev.vars holds the secrets
```

Then, from `tools/latency/` in another shell (see that directory's README for
`cloud_token.py`):

```sh
python cloud_token.py
python e2e_stress.py --relay http://127.0.0.1:8787 --token-file .cloud_token \
    --config incremental --mode manual --n 3
```

What proves it, in the run's own output and in its JSON:

* `usage_before` / `usage_after` — the relay's own `GET /v1/usage`, whose
  `words` climbs past 50 as the sessions run.
* the next session refused: `relay: close 4029/quota`, `"quota": true`, and
  the rows already measured kept rather than thrown away. With the grace, a
  session that is still running when the week passes 150 words is itself
  closed with `4029` after its crossing final.

The counter lives in the Durable Object's local storage, so it survives a
restart of `wrangler dev`; `rm -rf .wrangler` (git-ignored) is how a local
week is reset. There is deliberately no admin or reset route on the Worker —
a counter a client can clear is not a quota. The one route that clears it,
`DELETE /v1/account`, deletes the account with it and carries the week to the
next account with the same address. The real relay is
untouched by any of this: the local run never reaches it, and `--var` changes
nothing that is deployed.

## Tests

`npm test` runs the suite inside the real Workers runtime (Vitest +
`@cloudflare/vitest-pool-workers`), and `npm run typecheck` runs `tsc`. No key
and no network are needed: the suite mints its own ES256 key pair, serves its
JWKS to the Worker, and stands in for Sarvam and Supabase — including a fake
realtime socket, so the pipe, the word count and the `4029` refusal are all
exercised end to end.
