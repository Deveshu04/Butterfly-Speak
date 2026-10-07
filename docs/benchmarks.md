# Benchmarks

Latency measured against the live Sarvam service and the Butterfly Labs relay, and the tool that
scores formatting quality. Each section gives the commands that reproduce it.

The numbers come from one Windows 11 laptop on a residential connection in India. Percentiles
are nearest-rank, in milliseconds. The service's speed varies by the hour, so compare only runs
made back to back. The harness reads the cleanup prompt from the Rust source, so it measures the
prompt in the checkout it runs from.

The probes live in `tools/latency`; [its README](../tools/latency/README.md) explains every
option. They need `pip install websockets`, and every probe except the Cloud mode ones needs a
Sarvam key in `SARVAM_API_KEY`. Each run makes real, billed calls. Run the commands from
`tools/latency`.

## A short dictation, end to end

Measured on 7 October 2026. A real realtime session followed by a real polish call, on Bring your
own key, for a 13-word utterance (`utt_short.wav`). The clock starts when the finish frames are
sent. **Drain** is the wait for the last transcript, **polish** the chat call, **total** the two
together. The app's wait for the last audio after the key is released (at most 250 ms) and the
paste come on top.

| Mode | n | Drain p50 / p90 / p99 | Polish p50 / p90 / p99 | Total p50 / p90 / p99 |
|---|---|---|---|---|
| Push-to-talk | 50 | 200 / 236 / 332 | 281 / 320 / 1313 | 483 / 542 / 1517 |
| Hands-free | 30 | 201 / 220 / 280 | 284 / 317 / 450 | 477 / 531 / 727 |

No run missed Sarvam's `session.end`, lost the end marker, returned an empty transcript or
failed.

```
python e2e_stress.py --config shipped --mode manual --n 50
python e2e_stress.py --config shipped --mode vad --n 30
```

## Polish time by dictation length

Measured on 7 October 2026, right after the runs above. These runs skip the realtime session and
send paragraphs built from `tests/fixtures/earnings22.jsonl` (spontaneous earnings-call speech)
to the polish call, five paragraphs per length. The time to the first token stays near 150 ms at
every length; the rest of the call grows with the words.

- **One call** sends the whole text in one polish call after the key is released. The app never
  makes a single call over 400 words (`MAX_INPUT_WORDS` in `sarvam/chat.rs`); the ladder measures
  those lengths anyway.
- **Incremental** is what the app does: chunks of at least 50 words are polished while the user
  speaks, and only the tail is polished after release. The time shown is that tail call, the
  part the user waits for.

| Words | One call, p50 / p90 | Incremental, after release, p50 / p90 | Chunks polished while speaking | Prompt tokens per dictation |
|---|---|---|---|---|
| 100 | 863 / 968 | 372 / 453 | 1.0 | 3,738 |
| 200 | 1502 / 1793 | 289 / 540 | 2.8 | 7,344 |
| 300 | 2202 / 2381 | 308 / 542 | 4.6 | 10,978 |
| 400 | 3074 / 3187 | 409 / 676 | 6.2 | 14,224 |
| 500 | 3697 / 4016 | 370 / 448 | 8.0 | 17,856 |
| 600 | 4595 / 4762 | 693 / 814 | 9.4 | 20,720 |

No call in either ladder passed the app's 6-second polish budget or failed.

Ten 300-word dictations, run next: 355 / 553 / 1596 ms after release (p50 / p90 / p99), no
anomalous joins in 45 seams (a lowercase sentence start, a doubled full stop or a repeated
five-word run), and a difference from one whole-text polish of 0.051, below the 0.057 by which two
whole-text polishes of the same text differ from each other.

```
python e2e_stress.py --text-only --config shipped
python e2e_stress.py --text-only --config incremental
python e2e_stress.py --text-only --config incremental --words 300 --per-bucket 10
```

## Cloud mode

Measured on 24 September 2026. The relay's cost is a network round trip, independent of the
prompt. The same dictation was run straight to Sarvam and then through the relay, back to back,
ten times each, from a connection whose requests to `workers.dev` enter Cloudflare at Marseille.

| Path | Drain p50 / p90 | Polish p50 / p90 | Total p50 / p90 |
|---|---|---|---|
| Bring your own key | 194 / 241 | 315 / 488 | 510 / 689 |
| Cloud (relay) | 555 / 573 | 623 / 686 | 1131 / 1253 |

The relay added about 0.6 s (+620 ms at p50, +564 ms at p90).

```
python cloud_token.py
python e2e_stress.py --config shipped --mode manual --n 10
python e2e_stress.py --config shipped --mode manual --n 10 --relay https://butterflylabs-relay.butterflylabs.workers.dev --token-file .cloud_token
```

`cloud_token.py` is for the project's maintainers: sign-in to the Butterfly Labs project is
restricted, so it does not work for other accounts. Runs through the relay spend that account's
weekly words.

## Formatting quality

`fmtbench` scores the formatting engine against the 1,000 cases in `tests/fixtures` (200 each
from LibriSpeech-PC, Earnings-22, Earnings-22 Subset 10, Disfl-QA and IndicDiarBench): punctuation
error rate, casing F1, disfluency-removal F1 and a content-word WER that catches changed words.
Without `--live` it scores only the deterministic rules, with no network and no key. With
`--live` it also sends every case through the cloud polish path and the guardrail and reports
latency and token use; `--live` needs a Sarvam key in `SARVAM_API_KEY`. From `src-tauri`:

```
cargo run --release --bin fmtbench -- --fixtures ../tests/fixtures --level high --out report.md
cargo run --release --bin fmtbench -- --fixtures ../tests/fixtures --level high --live --model sarvam-105b --out report.md
```

`--limit N` scores only the first N cases of each source. The report states its method, its
commit and each source's licence and caveats.
