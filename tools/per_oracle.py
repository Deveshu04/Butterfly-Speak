#!/usr/bin/env python3
"""per_oracle.py — reference oracle for Punctuation Error Rate (PER).

PER is defined by the LibriSpeech-PC paper (Meister et al., NVIDIA,
ICASSP 2024):

    PER = (I_P + D_P + S_P) / (I_P + D_P + S_P + C_P)
    D_P = N_P,ref - (S_P + C_P)
    I_P = N_P,hyp - (S_P + C_P)

This script exists to cross-check `src-tauri/src/eval/per.rs`. It is a
manually-invoked reference — nothing imports it, no build step runs it, and
it is not on any CI path. The one automated caller is the `#[ignore]`d
`oracle` test module at the bottom of `per.rs`, which spawns this file and
diffs its counts against the Rust ones. Run it deliberately:

    cd src-tauri && cargo test --lib eval::per -- --ignored

Bare Python 3, stdlib only — nothing here needs `pip install`.

WHERE THE TWO AGREE (measured by those tests, not assumed)

The boundary is a Unicode-category one, and it is NOT "ASCII/Latin agrees,
the rest doesn't" — it cuts straight through ASCII. `re.findall` returns only
substrings matching one alternative of the regex below, so any character that
is neither `\\w`, an apostrophe, nor a listed mark is silently DROPPED here,
while the Rust tokenizer GLUES that same character into the adjacent word.

  * Agree exactly when every character of both strings is `\\w`-matchable, an
    ASCII apostrophe, whitespace, or a listed mark. Verified over 484 pairs
    spanning Latin, Cyrillic and CJK: zero divergence.
  * Diverge otherwise. Tokenization always differs; the COUNTS differ only
    when the extra/missing word tokens shift which marks the alignment pairs
    up, which needs the reference and hypothesis to differ in words too.
    Pure-ASCII counterexample, hyphen only: reference "a-b." against
    hypothesis "a-, b" gives S=1 in Rust and I=1,D=1 here. Worst measured
    pure-ASCII pair: '"ok", he said.' against "a-b, c." — rate 0.000 in Rust,
    0.667 here.

INDIC: THIS FILE REPRODUCES A NeMo BUG ON PURPOSE

`\\w` does not match the Unicode combining-mark categories Mn/Mc that
Devanagari vowel signs and virama are made of, and those sit *inside* words,
so dropping them splits the word: this oracle shatters "नमस्ते" into
["नमस", "त"] and "रिपोर्ट" into ["र", "प", "र", "ट"], exactly as NeMo does.
The Rust tokenizer keeps them whole and is the more correct of the two. That
is intentional divergence, not a defect to fix in either file — see the
module doc of `per.rs` and its
`devanagari_words_are_not_shattered_by_combining_marks` test.

Measured size of that divergence, because "they disagree on Indic" is not
actionable on its own: on a 400-pair Devanagari sweep whose sentences differ
in words as well as punctuation, corpus PER is 0.5886 in Rust vs 0.6308 here
— a 4.22-point gap. On Devanagari pairs where only the punctuation differs it
is 0.00 points, because identical words shatter identically on both sides and
the mark alignment survives. Always quote the corpus with the number.

Usage:
    echo '{"reference": "Hello, world.", "hypothesis": "Hello world."}' \\
        | python3 tools/per_oracle.py

    python3 tools/per_oracle.py --marks ".,?!;:" < fixtures.jsonl

    # A non-ASCII corpus needs ITS marks. Devanagari ends a sentence with the
    # danda U+0964 and CJK with U+3002; an ASCII mark set scores neither, and
    # the two examples above would report a flawless 0.000 on both.
    python3 tools/per_oracle.py --marks "।,?" < hindi.jsonl
    python3 tools/per_oracle.py --marks "。，？" < chinese.jsonl

Input: JSON Lines on stdin, one object per line with "reference" and
"hypothesis" string fields, decoded as UTF-8 (see NON-ASCII INPUT below); a
leading byte-order mark is skipped. Output: one JSON line of counts per input
line, plus (when more than one pair was read) a final aggregate line — the
sums across all pairs are what a corpus-level PER is computed from, per the
paper; it is not an average of per-sentence rates.

NON-ASCII INPUT: TWO WAYS TO AN UNEARNED PERFECT SCORE, BOTH REFUSED

Both are the documented invocation above aimed at a non-ASCII fixture, and a
0/0 rate prints as 0.000 — indistinguishable from flawless punctuation. Both
fail loudly instead; `per.rs`'s `oracle_scores_non_ascii_stdin_*` and
`oracle_refuses_a_mark_set_that_never_occurs_*` tests pin that.

  1. LOCALE-DECODED STDIN. `sys.stdin` decodes with the locale encoding on
     Python < 3.15 — cp1252 on a stock Windows box, which maps nearly every
     byte and so does NOT raise on UTF-8 input, it mojibakes it. A danda
     (U+0964) would arrive as three Latin-1 characters, match no entry in
     `marks`, and a Devanagari pair whose only error was that dropped danda
     would score C=S=I=D=0, rate 0.000, exit 0. So `_read_lines` reads
     `sys.stdin.buffer` and decodes UTF-8 itself, refusing anything else.
     JSON is UTF-8 by RFC 8259 §8.1, so nothing legitimate is lost.
  2. A MARK SET THAT DOES NOT OCCUR IN THE CORPUS — `--marks ".,?"` against
     Devanagari or CJK. Same all-zero counts, same 0.000. `main` exits
     non-zero when no requested mark appears anywhere in the whole input, and
     names the punctuation it did find so the fix is obvious. `--marks ""`
     is refused for the same reason, mirroring `per.rs`'s assert.

What this file still cannot see is a caller that mangles the text before it
arrives. Windows PowerShell 5.1 encodes native-command pipe input with
`$OutputEncoding`, which defaults to ASCII and turns every non-ASCII
character into "?" — and "?" is a legal mark, so the damage looks like data.
Redirect from a file (`< fixtures.jsonl`) or set
`$OutputEncoding = [Text.UTF8Encoding]::new($false)` first.
"""

import json
import re
import sys
import unicodedata

# ---------------------------------------------------------------------------
# Adapted from NVIDIA NeMo punct_er.py (Apache-2.0); see THIRD_PARTY_NOTICES.md.
#   nemo/collections/common/metrics/punct_er.py, class
#   OccurancePunctuationErrorRate, methods compute_operation_amounts /
#   compute_rates. https://github.com/NVIDIA-NeMo/NeMo
#
#   Copyright (c) 2023, NVIDIA CORPORATION & AFFILIATES. All rights reserved.
#   Licensed under the Apache License, Version 2.0 (the "License");
#   you may not use this file except in compliance with the License.
#   You may obtain a copy of the License at
#       http://www.apache.org/licenses/LICENSE-2.0
#   Unless required by applicable law or agreed to in writing, software
#   distributed under the License is distributed on an "AS IS" BASIS,
#   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#
# The tokenizer, DP table, backtrace and derived-insertions/deletions logic
# below are copied from that method almost line for line. What's trimmed for
# this file: the `tqdm` progress bar, the pandas/tabulate pretty-printer, and
# NeMo's own logging module — none of those are installed in a bare Python 3
# and none of them affect the numbers. The original also reports a
# per-punctuation-mark breakdown (Correct/Deletions/Insertions/Substitutions
# for "." separate from ","); this file sums that breakdown down to the same
# four aggregate counts `PunctCounts` uses on the Rust side, since the
# aggregate counts are all `per.rs` checks against.
# ---------------------------------------------------------------------------

COR, DEL, INS, SUB = "C", "D", "I", "S"

# Sentinel every mark collapses to before alignment — conceptually the same
# role as the "\u{0}PUNCT" mask string in per.rs, but a unique object here
# instead of a magic string: no `str` token can ever `==` it, so there is no
# collision to worry about even if `marks` were misconfigured.
MASK = object()


def tokenize(text, marks):
    """Split `text` into tokens using NeMo's own regex: word-like runs
    matching `[\\w']+`, or a single character from `marks`.

    This does NOT partition every character into a word or a mark —
    `re.findall` silently DROPS whatever matches neither alternative. A
    character that is not `\\w`, not an apostrophe, and not listed in
    `marks` disappears rather than joining a neighbouring word: tokenizing
    "a-b" with a `marks` set that excludes '-' yields `["a", "b"]`, not
    `["a-b"]` — the hyphen is simply gone, and the word became two tokens.
    Faithfully reproducing that is the point of this file; it is not a
    Rust-side difference to chase. It is also the ONE mechanism behind every
    divergence from `per.rs`, ASCII and Indic alike.

    Note this is a claim about tokens, not about scores. A dropped character
    only changes the final counts when the resulting token shift moves which
    marks the alignment pairs up — so many hyphenated pairs still score
    identically on both sides. The module docstring gives the measured
    numbers.

    `\\w` is Unicode-aware for *base* letters, so scripts written without
    combining marks — Cyrillic and CJK were both checked — tokenize the same
    as the Rust side. It excludes the combining-mark categories Mn/Mc, so
    Devanagari and similar scripts intentionally do NOT match the Rust side.
    The rule is the character's Unicode category, not its script, so Latin in
    decomposed (NFD) form is caught by it too: NFD "naive" with a combining
    diaeresis (n a i U+0308 v e) tokenizes to ["nai", "ve"] here, and NFD
    "cafe" with a combining acute to ["cafe"] — an accent inside a word
    splits it, a trailing one just vanishes.

    Each entry in `marks` must be a single character: multi-character
    "marks" silently degrade to their constituent characters once folded
    into the regex's character class, same limitation NeMo's own regex has.
    """
    marks_class = "".join(re.escape(m) for m in marks)
    return re.findall(rf"[\w']+|[{marks_class}]", text)


def _compute_operation_amounts(reference, hypothesis, marks):
    """Levenshtein-align masked token sequences, then demask at each
    correct-in-mask-space cell to recover real correct/substitution counts.
    Deletions and insertions are DERIVED afterwards, not read off the
    backtrace — see the module docstring."""
    r_tokens = tokenize(reference, marks)
    h_tokens = tokenize(hypothesis, marks)
    marks_set = set(marks)

    r_masked = [MASK if t in marks_set else t for t in r_tokens]
    h_masked = [MASK if t in marks_set else t for t in h_tokens]

    r_len, h_len = len(r_masked), len(h_masked)

    costs = [[0] * (h_len + 1) for _ in range(r_len + 1)]
    backtrace = [[COR] * (h_len + 1) for _ in range(r_len + 1)]

    for i in range(1, r_len + 1):
        costs[i][0] = i
        backtrace[i][0] = DEL
    for j in range(1, h_len + 1):
        costs[0][j] = j
        backtrace[0][j] = INS

    for i in range(1, r_len + 1):
        for j in range(1, h_len + 1):
            if r_masked[i - 1] == h_masked[j - 1]:
                costs[i][j] = costs[i - 1][j - 1]
                backtrace[i][j] = COR
            else:
                sub = costs[i - 1][j - 1] + 1
                ins = costs[i][j - 1] + 1
                de = costs[i - 1][j] + 1
                best = min(sub, ins, de)
                costs[i][j] = best
                if best == sub:
                    backtrace[i][j] = SUB
                elif best == ins:
                    backtrace[i][j] = INS
                else:
                    backtrace[i][j] = DEL

    correct = 0
    substitutions = 0
    i, j = r_len, h_len
    while i > 0 or j > 0:
        op = backtrace[i][j]
        if op == COR:
            if r_masked[i - 1] == MASK or h_masked[j - 1] == MASK:
                if r_tokens[i - 1] == h_tokens[j - 1]:
                    correct += 1
                else:
                    substitutions += 1
            i, j = i - 1, j - 1
        elif op == SUB:
            i, j = i - 1, j - 1
        elif op == INS:
            j -= 1
        else:  # DEL
            i -= 1

    n_ref = sum(1 for t in r_tokens if t in marks_set)
    n_hyp = sum(1 for t in h_tokens if t in marks_set)
    matched = correct + substitutions
    deletions = max(0, n_ref - matched)
    insertions = max(0, n_hyp - matched)

    return {
        "correct": correct,
        "substitutions": substitutions,
        "insertions": insertions,
        "deletions": deletions,
    }


def rate(counts):
    """1 - accuracy over punctuation slots; the denominator is punctuation
    *operations*, not reference length as in WER."""
    errors = counts["insertions"] + counts["deletions"] + counts["substitutions"]
    total = errors + counts["correct"]
    return 0.0 if total == 0 else errors / total


def punctuation_error_rate(reference, hypothesis, marks):
    counts = _compute_operation_amounts(reference, hypothesis, marks)
    counts["rate"] = rate(counts)
    return counts


# ---------------------------------------------------------------------------
# CLI driver (not part of the NeMo original): JSONL in, counts out.
# ---------------------------------------------------------------------------

DEFAULT_MARKS = list(".,?")  # matches MARKS in per.rs's test suite


def _print_doc():
    """Print the module docstring even where stdout cannot encode it.

    The docstring quotes Devanagari, and a Windows console defaults to
    cp1252: a plain `print(__doc__)` raises UnicodeEncodeError and emits a
    traceback instead of the help text. Degrade to `\\uXXXX` escapes for the
    characters the terminal cannot show rather than failing outright.
    """
    text = __doc__ or ""
    try:
        print(text)
    except UnicodeEncodeError:
        encoding = sys.stdout.encoding or "ascii"
        print(text.encode(encoding, errors="backslashreplace").decode(encoding))


def _describe(chars):
    """Render characters as pure-ASCII `U+XXXX NAME`.

    Diagnostics go to stderr, which on a stock Windows box encodes with
    cp1252 and would mangle the very characters the message is about — and
    `per.rs` asserts on this text, so it has to survive that trip intact.
    """
    return (
        ", ".join(
            "U+{:04X} {}".format(ord(c), unicodedata.name(c, "UNNAMED"))
            for c in sorted(chars)
        )
        or "(none)"
    )


def _parse_args(argv):
    marks = DEFAULT_MARKS
    args = argv[1:]
    while args:
        arg = args.pop(0)
        if arg == "--marks":
            if not args:
                raise SystemExit("--marks requires a value, e.g. --marks '.,?!'")
            marks = list(args.pop(0))
        elif arg in ("-h", "--help"):
            _print_doc()
            raise SystemExit(0)
        else:
            raise SystemExit(f"unrecognized argument: {arg}")

    if not marks:
        raise SystemExit(
            # Kept ASCII-only on purpose, like _describe: stderr encodes with
            # cp1252 on a stock Windows box and an em dash comes out as a
            # replacement character.
            "--marks is empty: nothing would tokenize to a mark, every count "
            "would be zero, and the rate would print as 0.000, a perfect "
            "score for text nobody scored. per.rs asserts on the same "
            "condition in punctuation_error_rate(). Pass at least one mark."
        )
    # argv reaches us already decoded. When the OS handed Python bytes it
    # could not decode (a non-UTF-8 POSIX locale), PEP 383 parks them in the
    # surrogate range, where they can never equal a character from stdin — so
    # every mark would silently miss. Refuse rather than score nothing.
    lone_surrogates = [c for c in marks if "\ud800" <= c <= "\udfff"]
    if lone_surrogates:
        raise SystemExit(
            "--marks contains bytes this Python could not decode "
            f"({_describe(lone_surrogates)}); they cannot match anything read "
            "from stdin, so every count would come back zero. Re-run with "
            "PYTHONUTF8=1 or under a UTF-8 locale."
        )
    return marks


def _read_lines():
    """Yield `(line_no, text)` from stdin, decoded as UTF-8 by us.

    NOT `for line in sys.stdin`. That decodes with the *locale* encoding on
    Python < 3.15, and cp1252 — the stock Windows default — maps almost every
    byte, so a UTF-8 fixture does not fail, it silently mojibakes: a danda
    becomes three Latin-1 characters that match no mark and get dropped by
    `tokenize`, and a Devanagari pair whose only error is a missing danda
    scores a flawless 0.000. Reading the raw byte stream sidesteps the locale
    entirely, and JSON is UTF-8 by RFC 8259 §8.1 regardless of it.

    Streams line by line rather than slurping: `per.rs` feeds this over a
    pipe from a writer thread while reading stdout, and buffering the whole
    input before emitting anything would change that timing for no gain.
    """
    stream = getattr(sys.stdin, "buffer", None)
    if stream is None:  # a replaced/text-only sys.stdin
        raise SystemExit(
            "sys.stdin has no binary buffer; this tool needs the raw bytes so "
            "it can decode UTF-8 regardless of the locale encoding"
        )
    first = True
    for line_no, raw in enumerate(stream, start=1):
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError as e:
            raise SystemExit(
                f"stdin line {line_no} is not valid UTF-8 ({e}). JSON is "
                "UTF-8 by RFC 8259; re-encode the fixture."
            )
        if first:
            # PowerShell redirects and Windows editors like to prepend one.
            if text.startswith("\ufeff"):
                text = text[1:]
            first = False
        yield line_no, text


def main(argv):
    marks = _parse_args(argv)
    marks_set = set(marks)

    total = {"correct": 0, "substitutions": 0, "insertions": 0, "deletions": 0}
    pairs = 0
    marks_seen = 0
    punct_seen = set()
    for line_no, line in _read_lines():
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
            reference = row["reference"]
            hypothesis = row["hypothesis"]
        except (json.JSONDecodeError, KeyError, TypeError) as e:
            raise SystemExit(f"stdin line {line_no}: expected {{'reference': ..., 'hypothesis': ...}} JSON, got: {e}")

        counts = punctuation_error_rate(reference, hypothesis, marks)
        print(json.dumps(counts))
        for k in total:
            total[k] += counts[k]
        pairs += 1
        for ch in reference + hypothesis:
            if ch in marks_set:
                marks_seen += 1
            elif marks_seen == 0 and unicodedata.category(ch).startswith("P"):
                # Only ever read by the refusal below, so stop paying for the
                # category lookup as soon as one real mark has turned up.
                punct_seen.add(ch)

    if pairs > 1:
        total["rate"] = rate(total)
        print(json.dumps({"total": total, "pairs": pairs}))

    # A mark set that never occurs is not a corpus with clean punctuation; it
    # is a misconfigured run. Every count is zero, `rate` is a genuine 0/0,
    # and it prints as 0.000 next to real scores — the exact failure `per.rs`
    # documents on `PunctCounts::rate` and asserts against for an empty mark
    # set. The lines above are already on stdout, so say this on stderr AND
    # exit non-zero: whoever piped us into something must not read that 0.000
    # as a result. Deliberately corpus-wide, not per line — a single
    # punctuation-free sentence inside a real corpus is ordinary.
    if pairs and marks_seen == 0:
        raise SystemExit(
            f"read {pairs} pair(s) and not one of the marks in --marks occurs "
            "in any of them, so every count is zero and that prints as a "
            "flawless rate of 0.000. Refusing to pass it off as a score.\n"
            f"  --marks asked for: {_describe(marks_set)}\n"
            f"  punctuation found: {_describe(punct_seen)}\n"
            "Pass the marks the corpus actually uses (Devanagari sentences "
            "end in U+0964, CJK in U+3002), or check that the fixture is the "
            "one you meant."
        )


if __name__ == "__main__":
    main(sys.argv)
