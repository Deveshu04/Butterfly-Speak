# Latency probes

Scripts that measure the dictation path's latency against Sarvam's live API, and the gate for
changes to that path. They call the real service, so every run costs real calls on the key or
account they use.

| Script | What it measures |
| --- | --- |
| `ws_probe.py` | One Saaras realtime session per scenario, with every server frame timestamped against the client's finish frame: how long after `speech_end` the final arrives, whether `end` (rather than `flush`) produces a prompt `session.end`, whether finals arrive mid-stream under manual endpointing, and how close the last partial is to the final. |
| `polish_probe.py` | The AI Polish chat call alone, in the app's request shape, split into connection, time to first token and decode, across three prompt sizes and three utterance lengths, plus the cost of the end marker and of leaving reasoning on. About 30 calls; writes `polish_probe_results.json` into the working directory. |
| `e2e_stress.py` | End to end, N times: a realtime session followed by a polish call, in the app's current request shape (`shipped`) and an older one kept for comparison (`legacy`). Reports p50/p90/p99 of drain, polish and total, and `session.end` misses. This is the gate. It also has a text-only mode for long dictations and a simulation of incremental polish (below). |
| `cloud_token.py` | Signs in from the command line with the app's PKCE flow and writes the access token `e2e_stress.py --relay` uses. It measures nothing. |
| `make_fixtures.ps1` | Regenerates the speech fixtures `utt_short.wav` and `utt_long.wav` with the built-in Windows voice (16 kHz, 16-bit, mono). |

## Setup

1. Python 3 and `pip install websockets`. Everything else is in the standard library.
2. Set `SARVAM_API_KEY` to a Sarvam API key from [dashboard.sarvam.ai](https://dashboard.sarvam.ai).
   Every script needs it except `cloud_token.py` and `e2e_stress.py --relay`. None of them read
   the key the app stores.
3. For anything that uses `utt_long.wav`, run `powershell -File tools\latency\make_fixtures.ps1`
   once. `utt_short.wav` is committed; `utt_long.wav` (about 35 s) is generated.

Run the scripts from `tools/latency/`. `e2e_stress.py` prints and stores counts, timings and
ratios only, never the text it sends or the replies. The two probes show text from their fixed
test inputs: `ws_probe.py` prints the start of each partial and final transcript, and
`polish_probe.py` prints up to 600 characters of each reply and saves the full replies in
`polish_probe_results.json`.

## ws_probe.py

```
python ws_probe.py            # scenarios A to F
python ws_probe.py B D        # only some
```

Scenarios A to C stream `utt_short.wav` with manual endpointing and different finish frames; D
streams `utt_long.wav` the same way; E and F stream it with the server's voice detection.
`BS_STREAM_TYPE` (`balanced` or `fast`), `BS_LANG`, `BS_MODE` and `BS_RT_MODEL` change the
session's parameters.

## The gate

Six runs, both configurations:

```
python e2e_stress.py --config legacy  --mode manual --n 50
python e2e_stress.py --config shipped --mode manual --n 50
python e2e_stress.py --config legacy  --mode vad    --n 30
python e2e_stress.py --config shipped --mode vad    --n 30
python e2e_stress.py --config shipped --mode vad    --n 5  --wav utt_long.wav
python e2e_stress.py --config shipped --mode manual --n 15 --concurrency 5
```

The `utt_long.wav` run covers a dictation in three parts with two 1.5 s pauses. The
`--concurrency 5` run opens a chat connection per iteration, so its polish numbers include
connection setup: compare its drain with the sequential run, not its polish.

## Long dictations

```
python e2e_stress.py --text-only --config shipped
python e2e_stress.py --text-only --config legacy --words 200,400 --per-bucket 3
```

`--text-only` skips the realtime session and sends the polish call paragraphs built from
`tests/fixtures/earnings22.jsonl`, lowercased and unpunctuated. `--words` sets the input
lengths (default `100,200,300,400,500,600`) and `--per-bucket` the paragraphs per length
(default 5). Each sample records the polish time, time to first token, completion tokens, the
output/input word ratio, whether the end marker came back, whether the call took longer than
the app's 6 s polish timeout, and whether the app would skip polish at that length (over 400
words). Results go to `e2e_stress_textonly_<config>.json`.

## Incremental polish

```
python e2e_stress.py --text-only --config incremental
python e2e_stress.py --text-only --config incremental --words 300 --per-bucket 10
python e2e_stress.py --text-only --config incremental --words 300 --per-bucket 10 \
    --context-rule v1 --no-repair --tag v1-norepair
```

This polishes the same lengths the way the app polishes long dictations, from the fixture's
punctuated text, lowercased. A Python copy of `sarvam::incremental::Segmenter` closes chunks as
sentences arrive; each chunk is polished with the polished text before it as context and timed
as `background_*`, since the app makes these calls while you are still speaking; only the last
call, the tail, counts as `critical_ms`, the wait after the key is released. Each polished piece
goes through a copy of `sarvam::incremental::repair_seam` before it is joined.

- `--no-repair` skips the seam repair, so the seam counts describe the model alone.
- `--context-rule v1|v2` picks the context instruction: `v2` is the app's, `v1` an earlier
  wording kept for comparison.
- `--tag NAME` adds `_NAME` to the output filename.

Each sample counts the seams in the joined text (a lowercase start after a finished sentence, a
doubled sentence mark, or the model repeating its context), before and after the repair. Where
the input fits one call, the whole text is also polished twice in single calls:
`parity_diff_ratio` compares the joined text with the first of those replies, and
`parity_noise_floor` compares the two replies with each other, so read the two together.
`context_echoes` and `over_expansion` count replies that repeated their context or would have
failed the app's length check. Results go to `e2e_stress_textonly_incremental.json`.

## Cloud mode

`--relay URL --token-file PATH` sends any of the runs above through a Butterfly Labs relay
instead of straight to Sarvam. The query and request bodies are the same; only the host changes,
and the credential becomes `Authorization: Bearer <access token>`. No Sarvam key is read.

This needs:

- a relay to measure, either a deployment you can use or one run locally as `relay/README.md`
  describes;
- access to a Supabase project the relay accepts tokens from, with Google sign-in enabled,
  `http://127.0.0.1:8765/callback` in its Authentication → URL Configuration → Redirect URLs,
  and an account that can sign in to it. `cloud_token.py` uses the app's project unless
  `BS_SUPABASE_URL` and `BS_SUPABASE_ANON_KEY` are both set to another.

```
python cloud_token.py                      # opens your browser, writes .cloud_token
python e2e_stress.py --relay https://<relay> --token-file .cloud_token \
    --config shipped --mode manual --n 50
```

The token lasts about an hour; run `cloud_token.py` again to renew it. The token file is
git-ignored and written so that only your user can read it. `python cloud_token.py --self-test`
checks the PKCE code and the file writing without a network.

To see what a run would send without connecting to anything, add `--dry-run`:

```
python e2e_stress.py --relay https://<relay> --token-file .cloud_token \
    --config shipped --mode manual --n 1 --dry-run
```

A `401` stops the run with `token rejected - rerun cloud_token.py`. Reaching the relay's weekly
limit (close code `4029`, reason `quota`) stops it cleanly, records `"quota": true` and keeps the
rows measured so far. Relay runs also record the relay's `GET /v1/usage` count before and after.

To compare the two lanes, run the gate directly and then with `--relay … --token-file …`
appended, back to back: the difference between the p90s is what the relay hop costs. Test the
weekly limit against a local relay (see **Proving the quota locally** in `relay/README.md`), not
a deployed one, where the account would stay locked until Monday.
