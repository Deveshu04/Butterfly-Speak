"""End-to-end latency stress harness: the real Saaras WebSocket followed by
the real polish call, N times, for the older session and request shape
("legacy") and the one the app ships ("shipped"), measured against the live
service rather than projected.

    python tools/latency/e2e_stress.py --config legacy  --mode manual --n 50
    python tools/latency/e2e_stress.py --config shipped --mode manual --n 50
    python tools/latency/e2e_stress.py --config legacy  --mode vad    --n 30
    python tools/latency/e2e_stress.py --config shipped --mode vad    --n 30
    python tools/latency/e2e_stress.py --config shipped --mode manual --n 15 --concurrency 5
    python tools/latency/e2e_stress.py --text-only --config shipped
    python tools/latency/e2e_stress.py --text-only --config incremental

Each burst iteration uses its own chat connection, so burst polish times
include a connection setup — compare burst drain, not burst polish, against
the sequential run.

--text-only skips the WebSocket and feeds long *unformatted* paragraphs
(concatenated from tests/fixtures/earnings22.jsonl, which is lowercased and
unpunctuated) straight to the polish call, one ladder of input lengths
(--words, default 100,200,300,400,500,600) x --per-bucket samples, ~1 s
apart. That is where the app's formatting fails: sarvam::chat skips the
polish above MAX_INPUT_WORDS = 400 and abandons it after POLISH_TIMEOUT = 6 s,
so the ladder measures both walls. Reports per bucket and writes
e2e_stress_textonly_<config>.json. Word counts and timings only — the
paragraphs and the model's replies are never printed or stored.

--text-only --config incremental simulates incremental polish on the same
ladder: the paragraphs are the fixtures' *target* text lowercased
(punctuated, uncased — what Saaras actually hands the app), a Python mirror of
sarvam::incremental::Segmenter is fed one sentence at a time as if finals were
arriving and cuts them into >=50-word chunks at sentence ends, each chunk is
polished with the polished text before it as <before_cursor> context (timed,
but off the critical path: in the app it happens while the user is still
speaking), and only the tail is timed as the critical path — what the user
waits for after the key is released. Every polished piece then goes through a
mirror of the deterministic seam repair (strip an echoed run, capitalise a
sentence start) before it is assembled, unless --no-repair. It checks the
seams of the assembled text after the repair, counts the replies that echoed
their context, and, where the whole text still fits in one call, diffs
whole-vs-chunked word by word — running that call twice, so the model's own
run-to-run difference is reported next to it as a noise floor.
--context-rule v1|v2 chooses which <before_cursor> rule the context calls
carry, and --tag keeps the JSON of two such runs apart.

--relay URL --token-file PATH runs everything down the app's Cloud lane
instead: the same realtime query on the relay's /v1/realtime and the same chat
body on its /v1/chat/completions, carrying the user's Supabase access token
(tools/latency/cloud_token.py writes one) instead of a Sarvam key, which this
mode neither reads nor needs. --dry-run prints the exact URLs and headers it
would use and connects to nothing.

    python tools/latency/cloud_token.py
    python tools/latency/e2e_stress.py --relay https://<relay> \
        --token-file tools/latency/.cloud_token --config incremental --mode manual --n 3

Per iteration: one WebSocket session streaming utt_short.wav in real time,
the config's finish frames, the config's drain-exit rule, then one chat
completion with the config's marker and streaming setting. Reports
p50/p90/p99 (nearest rank, as format::timing::percentiles) of drain,
polish, total, plus session.end misses. Needs SARVAM_API_KEY, `websockets`,
and utt_short.wav next to this file (see ws_probe.py for the WAV recipe).
"""
import argparse
import asyncio
import base64
import difflib
import http.client
import json
import os
import ssl
import sys
import time
import urllib.parse
import uuid
import wave

import websockets

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

# --- Cloud mode: the Butterfly Labs relay ------------------------------------
# `--relay URL --token-file PATH` is the app's Cloud lane: the same realtime
# query and the same chat body, on the relay's own routes, authenticated with
# the user's Supabase access token instead of a Sarvam key. It is read out of
# argv here, ahead of the argument parser, because the two statements below it
# depend on it — `polish_probe` exits at import time without SARVAM_API_KEY,
# and Cloud mode has no Sarvam key to give it. The relay holds the only one.
RELAY_REALTIME_PATH = "/v1/realtime"       # sarvam::RELAY_REALTIME_PATH
RELAY_CHAT_PATH = "/v1/chat/completions"   # format::backend::RELAY_CHAT_PATH
RELAY_USAGE_PATH = "/v1/usage"             # format::backend::RELAY_USAGE_PATH
CLOSE_QUOTA = 4029                         # relay/src/user_session.ts


def flag_value(argv, name):
    """`--name value` or `--name=value` out of raw argv, or None. Deliberately
    literal: argparse expands `--rel` to `--relay` and this does not, so an
    abbreviation is refused here rather than running half the harness in one
    lane and half in the other."""
    for arg in argv:
        head = arg.split("=", 1)[0]
        if head.startswith("--") and head != name and name.startswith(head):
            sys.exit(f"spell {name} in full: it is read before the argument parser runs")
    for i, arg in enumerate(argv):
        if arg == name and i + 1 < len(argv):
            return argv[i + 1]
        if arg.startswith(name + "="):
            return arg.split("=", 1)[1]
    return None


def relay_ws_base(base):
    """`sarvam::ws_scheme` + `Lane::realtime_endpoint`: the relay's realtime
    route with the scheme a WebSocket wants. Anything that is not http(s) is
    handed back untouched, as the Rust does, so one bad URL produces one
    error rather than two different ones."""
    if base.startswith("https://"):
        return "wss://" + base[len("https://"):] + RELAY_REALTIME_PATH
    if base.startswith("http://"):
        return "ws://" + base[len("http://"):] + RELAY_REALTIME_PATH
    return base + RELAY_REALTIME_PATH


def relay_target(base):
    """(host, port, tls, path prefix) for the relay's HTTP routes. `http://`
    is how a local `wrangler dev` is reached; everything else is TLS."""
    u = urllib.parse.urlsplit(base)
    tls = u.scheme != "http"
    return u.hostname or "", u.port or (443 if tls else 80), tls, u.path.rstrip("/")


class TokenRejected(Exception):
    """The relay refused the sign-in token (401). One notice, then stop:
    every following call would be refused for the same reason."""


class QuotaExhausted(Exception):
    """The relay closed the realtime socket with 4029/`quota`: this week's
    words are spent. The run stops cleanly and keeps what it measured."""


# The relay base URL (no trailing slash) and the access token, both empty in
# direct mode. TOKEN is filled from --token-file once argparse has run; it is
# never printed, logged or written to the run JSON.
RELAY = (flag_value(sys.argv[1:], "--relay") or "").rstrip("/")
TOKEN = ""
if RELAY:
    # Only to get `polish_probe` past its import-time SARVAM_API_KEY check.
    # `polish_probe.KEY` is blanked immediately after, so no Sarvam header can
    # be built from it, and `RelayConn` strips that header in any case.
    os.environ.setdefault("SARVAM_API_KEY", "unused-in-cloud-mode")

import polish_probe  # noqa: E402  (prompt fragments + PROMPTS["HIGH(shipped)"])

if RELAY:
    polish_probe.KEY = ""

KEY = "" if RELAY else os.environ["SARVAM_API_KEY"].strip()
# The realtime endpoint before the query string, which is identical on both
# lanes — `sarvam::Lane::realtime_endpoint` makes exactly this choice.
WS_BASE = relay_ws_base(RELAY) if RELAY else "wss://api.sarvam.ai/speech-to-text-realtime/ws"
CHAT_HOST = "api.sarvam.ai"
CHAT_PATH = "/v1/chat/completions"
CHUNK = 3200  # 100 ms of PCM16 @ 16 kHz
# sarvam::ws::IDLE_GRACE. The quiet window itself is the config's `quiet_ms`
# (300 ms = sarvam::ws::QUIET_WINDOW for both configs).
IDLE_GRACE = 0.8
# Spontaneous earnings-call speech, lowercased and unpunctuated: the closest
# thing in the repo to what a long dictation hands the polish model.
FIXTURE = os.path.join(HERE, "..", "..", "tests", "fixtures", "earnings22.jsonl")
# sarvam::chat: above MAX_INPUT_WORDS the app pastes raw text without polishing,
# and a polish that outlives POLISH_TIMEOUT is abandoned for the rule-cleaned text.
MAX_INPUT_WORDS = 400
POLISH_TIMEOUT_MS = 6000
# sarvam::incremental, verbatim: a chunk is handed out only once that many
# words have closed a sentence; a run-on past the ceiling is cut at its last
# comma; the context shown to the model is the last CONTEXT_MAX_CHARS of the
# polished text so far.
CHUNK_MIN_WORDS = 50
MAX_CHUNK_WORDS = 120
CONTEXT_MAX_CHARS = 600
TERMINALS = ".!?।॥"          # . ! ? । ॥
CLOSERS = "\"'”’)]»"    # " ' ” ’ ) ] »
# How long to wait between the calls of one simulated dictation. In the app
# they are >= 50 words of speech apart (~20 s); here they would otherwise be
# back to back, which is not a load pattern the app ever produces.
CHUNK_PACE_S = 0.25
# Which <before_cursor> rule the context-carrying calls carry ("v2" is what
# ships; "v1" is an earlier wording, kept so the two can be compared), and
# whether the deterministic seam repair runs. Both are set from the command
# line by run_text_only.
CONTEXT_RULE = "v2"
REPAIR = True

# The end-marker rule, per config. `shipped` and `incremental` send the app's
# own (`polish_probe.END_MARKER_RULE`). `legacy` keeps the older request's
# shape, with the marker straight after the text's last character instead of
# on a line of its own: that shape is what drops the sentence's final
# punctuation, so the comparison needs it. `{m}` is the marker.
END_MARKER_RULE_SAME_LINE = (
    "The app deletes the end marker below before anyone reads your reply, so it can sit "
    "right against your text: copy it onto the end of your last line, with nothing between, "
    "and stop there:\n{m}")

CONFIGS = {
    # The older shape: flush to finish, a UUID marker, no streaming.
    "legacy": {"finish_manual": ["speech_end", "flush"], "finish_vad": ["flush"],
               "quiet_ms": 300, "hard_floor_ms": 4000, "marker": "uuid", "stream": False,
               "end_marker_rule": END_MARKER_RULE_SAME_LINE},
    # What the app ships: end at finish, exit on session.end, short marker,
    # streaming.
    "shipped": {"finish_manual": ["speech_end", "end"], "finish_vad": ["end"],
                "quiet_ms": 300, "hard_floor_ms": 4000, "marker": "short", "stream": True,
                "end_marker_rule": polish_probe.END_MARKER_RULE},
    # `shipped`'s session and request shape exactly — only the text-only
    # ladder differs, where `incremental` turns one call over the whole
    # dictation into background chunks plus a tail call.
    "incremental": {"finish_manual": ["speech_end", "end"], "finish_vad": ["end"],
                    "quiet_ms": 300, "hard_floor_ms": 4000, "marker": "short", "stream": True,
                    "end_marker_rule": polish_probe.END_MARKER_RULE, "incremental": True},
}


def pcm(path):
    with wave.open(path, "rb") as w:
        assert (w.getframerate(), w.getnchannels(), w.getsampwidth()) == (16000, 1, 2), w.getparams()
        return w.readframes(w.getnframes())


def ws_url(endpointing, stream_type):
    return (f"{WS_BASE}?model=saaras:v3-realtime&language_code=en-IN&stream_type={stream_type}"
            f"&mode=transcribe&endpointing={endpointing}&encoding=linear16&sample_rate=16000")


def percentiles(values):
    if not values:
        return None  # no samples — a null, not three zeroes that read as 0 ms
    s = sorted(values)
    pick = lambda q: s[max(1, int(-(-q * len(s) // 1))) - 1]  # ceil(q*n), nearest rank
    return (pick(0.50), pick(0.90), pick(0.99))


# --- one credential per lane, as sarvam::Transport --------------------------


def realtime_headers():
    """The single header the upgrade carries, `sarvam::Transport::auth_header`
    lane for lane: Sarvam's own `api-subscription-key` on the
    Bring-your-own-key lane, `Authorization: Bearer <the user's Supabase access
    token>` on the Cloud lane, and never both — the app has no Sarvam key to
    send the relay, and the relay authenticates the user, not the key."""
    return {"Authorization": f"Bearer {TOKEN}"} if RELAY else {"api-subscription-key": KEY}


class RelayConn:
    """A chat connection with the relay's credential on it.

    `polish_probe.call` writes Sarvam's path and Sarvam's headers; this
    rewrites both on the way out and leaves the *body* exactly as it was,
    which is the part that has to match — the relay forwards the body untouched,
    so Sarvam receives the same bytes on either lane. Wrapping the connection
    rather than copying the call also means one piece of code does the SSE
    parsing and the timing for both lanes, so a Cloud number and a
    Bring-your-own-key number are measured the same way."""

    def __init__(self, conn, path):
        self._conn = conn
        self._path = path

    def request(self, method, path, body=None, headers=None):
        headers = {k: v for k, v in (headers or {}).items()
                   if k.lower() != "api-subscription-key"}
        headers["Authorization"] = f"Bearer {TOKEN}"
        self._conn.request(method, self._path, body=body, headers=headers)

    def getresponse(self):
        return self._conn.getresponse()

    def close(self):
        self._conn.close()


def chat_conn():
    """A fresh connection to the chat host: Sarvam's in direct mode, the
    relay's in Cloud mode (plain HTTP when it is a local `wrangler dev`)."""
    if RELAY:
        host, port, tls, prefix = relay_target(RELAY)
        conn = (http.client.HTTPSConnection(host, port, timeout=60,
                                            context=ssl.create_default_context())
                if tls else http.client.HTTPConnection(host, port, timeout=60))
        return RelayConn(conn, prefix + RELAY_CHAT_PATH)
    return http.client.HTTPSConnection(CHAT_HOST, 443, timeout=60, context=ssl.create_default_context())


def load_token(path):
    """The access token from `--token-file`. Whitespace-stripped, because a
    trailing newline is a 401 nobody can see, and never printed."""
    try:
        with open(path, encoding="utf-8") as f:
            token = f.read().strip()
    except OSError:
        raise SystemExit(f"--token-file: cannot read {path} - run cloud_token.py first")
    if not token:
        raise SystemExit(f"--token-file: {path} is empty - run cloud_token.py first")
    return token


def relay_usage():
    """`GET /v1/usage` — the relay's own weekly counter, which is what makes a
    quota run provable rather than asserted. Returns the parsed body, or a
    dict naming what went wrong instead."""
    host, port, tls, prefix = relay_target(RELAY)
    conn = (http.client.HTTPSConnection(host, port, timeout=30,
                                        context=ssl.create_default_context())
            if tls else http.client.HTTPConnection(host, port, timeout=30))
    try:
        conn.request("GET", prefix + RELAY_USAGE_PATH,
                     headers={"Authorization": f"Bearer {TOKEN}",
                              "Accept": "application/json"})
        resp = conn.getresponse()
        raw = resp.read().decode("utf-8", "replace")
        if resp.status == 401:
            raise TokenRejected()
        if resp.status != 200:
            return {"status": resp.status}
        return json.loads(raw)
    except TokenRejected:
        raise
    except Exception as e:
        # Type only, as everywhere here: the message can carry a URL.
        return {"error": type(e).__name__}
    finally:
        conn.close()


def run_meta():
    """What lane this run went down, for the run JSON. Empty in direct mode,
    so the shipped ladders' files keep exactly the shape they have."""
    return {"transport": "relay", "relay": RELAY} if RELAY else {}


# --- sarvam::incremental, mirrored ------------------------------------------
# Rule for rule with the Rust segmenter, so what this harness measures is
# what the app does. Rust indexes bytes and Python characters; the
# arithmetic is identical for both. CONTEXT_MAX_CHARS is
# counted in *characters* on both sides — `incremental::context_tail` walks
# back over `char_indices`, not bytes — so the two windows hold the same text
# in Devanagari as in Latin.


def word_count(s):
    return len(s.split())


def last_sentence_end(text):
    """Index just past the last sentence end (the terminal mark plus any
    closing quotes/brackets), or None. A `.` between two digits is a decimal
    point, not a sentence end."""
    n = len(text)
    for i in range(n - 1, -1, -1):
        c = text[i]
        if c not in TERMINALS:
            continue
        if c == ".":
            prev_digit = i > 0 and text[i - 1] in "0123456789"
            next_digit = i + 1 < n and text[i + 1] in "0123456789"
            if prev_digit and next_digit:
                continue
        end = i + 1
        j = i + 1
        while j < n and text[j] in CLOSERS:
            end = j + 1
            j += 1
        return end
    return None


class Segmenter:
    """Tracks how much of the joined finals has been handed out for polishing.
    The handed prefix is kept as text, not an offset, so a joined text that no
    longer starts with it stops chunking instead of slicing at a stale point."""

    def __init__(self):
        self.handed = ""
        self.chunks = 0
        self.desynced = False

    def _remainder(self, joined):
        if self.desynced or not joined.startswith(self.handed):
            return joined
        return joined[len(self.handed):].lstrip()

    def take_chunk(self, joined):
        if self.desynced:
            return None
        if not joined.startswith(self.handed):
            self.desynced = True
            return None
        rem = self._remainder(joined)
        if word_count(rem) < CHUNK_MIN_WORDS:
            return None
        end = last_sentence_end(rem)
        if end is not None and word_count(rem[:end]) >= CHUNK_MIN_WORDS:
            cut = end
        elif word_count(rem) > MAX_CHUNK_WORDS:
            # Run-on fallback: last comma, else last space.
            comma = rem.rfind(",")
            if comma != -1 and word_count(rem[:comma + 1]) >= CHUNK_MIN_WORDS:
                cut = comma + 1
            else:
                space = rem.rfind(" ")
                cut = len(rem) if space == -1 else space
        else:
            return None
        chunk = rem[:cut].strip()
        if not chunk:
            return None
        self.handed = joined[:len(joined) - len(rem) + cut]
        self.chunks += 1
        return chunk

    def tail(self, joined, partial=""):
        rem = self._remainder(joined).strip()
        partial = partial.strip()
        return " ".join(p for p in (rem, partial) if p)

    def chunks_taken(self):
        return self.chunks


def context_tail(polished):
    """The last CONTEXT_MAX_CHARS characters of `polished`, cut forward to a
    word boundary — the text before the cursor the next chunk is shown."""
    if len(polished) <= CONTEXT_MAX_CHARS:
        return polished
    start = len(polished) - CONTEXT_MAX_CHARS
    space = polished.find(" ", start)
    return polished[start:] if space == -1 else polished[space + 1:]


# format::guard rejects a reply as over-expansion when
# output_words > input_words * MAX_RATIO + EXPANSION_SLACK_WORDS, and the app
# then keeps the rule-cleaned text. The harness does not apply the rejection —
# it measures the model, not the guard — but it counts it, because a reply that
# echoes its <before_cursor> context is exactly what trips it.
GUARD_MAX_RATIO = 2.0
GUARD_EXPANSION_SLACK_WORDS = 4


def over_expanded(source, reply):
    return word_count(reply) > word_count(source) * GUARD_MAX_RATIO + GUARD_EXPANSION_SLACK_WORDS


EDGE_PUNCT = TERMINALS + CLOSERS + ",;:—–-"


def bare_word(w):
    """One word with its casing and edge punctuation dropped. This is the
    loose normaliser behind the `echoes_context` and `seam_check` heuristics
    only; the seam repair uses `norm_word`, which mirrors the Rust."""
    return w.strip(EDGE_PUNCT).lower()


def bare_words(s):
    """Words with their casing and edge punctuation dropped — what to compare
    when asking whether text reappeared, since both are what a formatter is
    allowed to change."""
    return [bare_word(w) for w in s.split()]


def echoes_context(context, reply):
    """True when any 5-gram of the text sent as <before_cursor> reappears in
    the reply: the model writing out again what is already in the document,
    which the before-cursor rule forbids. The seam check sees this only when
    the repeat lands within 40 words of the join; this sees it wherever it
    lands, and it is the failure mode the rule exists to prevent."""
    if not context or not reply:
        return False
    c, o = bare_words(context), bare_words(reply)
    if len(c) < 5 or len(o) < 5:
        return False
    grams = {tuple(c[i:i + 5]) for i in range(len(c) - 4)}
    return any(tuple(o[i:i + 5]) in grams for i in range(len(o) - 4))


def assemble_polished(pieces):
    """Polished chunks then tail, single spaces, empties skipped."""
    return " ".join(p.strip() for p in pieces if p and p.strip())


# --- sarvam::incremental::repair_seam, mirrored ----------------------------
# The deterministic half of the seam fix: run over every polished chunk (and
# the tail) before it is appended, so the outcome does not rest on the model
# obeying the before-cursor rule. It can only delete text that is already in
# the document word for word, and only add an upper case letter.
ECHO_MIN_WORDS = 5


def norm_word(w):
    """`sarvam::incremental::norm_word`: *every* non-alphanumeric character
    dropped, then lower-cased. Stripping only the edges is not enough — the
    model rewrites `don't` as `don’t`, and the two must still compare equal or
    the echo goes undetected. Devanagari matras and the virama are not
    alphanumeric in either language, so both sides drop them identically."""
    return "".join(c for c in w if c.isalnum()).lower()


def word_spans(s):
    """(start, end) of every whitespace-separated word in `s`."""
    spans, i, n = [], 0, len(s)
    while i < n:
        if s[i].isspace():
            i += 1
            continue
        j = i
        while j < n and not s[j].isspace():
            j += 1
        spans.append((i, j))
        i = j
    return spans


def repair_seam(previous, chunk, inp):
    """`sarvam::incremental::repair_seam`, rule for rule. Returns (repaired,
    echoed_words, strips, capitalised).

    `inp` is the text the model was *given* for this piece (the chunk, or the
    tail). It is what tells an echo the model added from a repetition the user
    actually dictated.

    1. Echo strip: a run of >= 5 words at the start of `chunk` whose
       `norm_word` forms equal, word for word, a *suffix* of `previous` is
       dropped together with the leading characters of what follows that are
       neither alphanumeric nor a quote — the Rust predicate, so an opening
       `"` or `'` survives and an opening `(`, `[` or `«` does not. Applied at
       most twice; the longest matching run wins. Two declines, each counted
       as nothing: a strip that would leave no alphanumeric character at all
       (so a chunk can never be emptied), and a run that `inp` itself opens
       with (so a phrase the user repeated is never deleted).
    2. Sentence-start casing: when `previous` is empty or `last_sentence_end`
       lands on its very end, a first alphabetic character that is a lowercase
       ASCII letter is upper-cased — unless its own word carries an upper case
       letter after that character ("iPhone", "eBay"), which is spelled that
       way on purpose. Devanagari has no case and is untouched."""
    out = chunk.strip()
    echoed, strips = 0, 0
    prev_norm = [w for w in (norm_word(t) for t in previous.split()) if w]
    inp_norm = [w for w in (norm_word(t) for t in inp.split()) if w]
    for _ in range(2):
        spans = word_spans(out)
        chunk_norm = [norm_word(out[a:b]) for a, b in spans]
        k = 0
        for n in range(min(len(prev_norm), len(chunk_norm)), ECHO_MIN_WORDS - 1, -1):
            if chunk_norm[:n] == prev_norm[len(prev_norm) - n:]:
                k = n
                break
        if not k:
            break
        # The user dictated the repetition: the words the model was given open
        # with the very run that matched, so the model added nothing.
        if len(inp_norm) >= k and inp_norm[:k] == chunk_norm[:k]:
            break
        i = spans[k][0] if k < len(spans) else len(out)
        while i < len(out) and not out[i].isalnum() and out[i] not in ("\"", "'"):
            i += 1
        rest = out[i:].lstrip()
        # Only text already in the document is ever removed — and never all of
        # it: a reply that is nothing but an echo ships as it came rather than
        # vanishing at the seam.
        if not any(c.isalnum() for c in rest):
            break
        out = rest
        echoed += k
        strips += 1

    capitalised = 0
    prev = previous.rstrip()
    if not prev or last_sentence_end(prev) == len(prev):
        for i, c in enumerate(out):
            if c.isalpha():
                # "iPhone", "eBay": an upper case letter later in the same
                # word means the word is spelled that way on purpose.
                end = next((b for a, b in word_spans(out) if a <= i < b), len(out))
                camel = any(ch.isupper() for ch in out[i + 1:end])
                if "a" <= c <= "z" and not camel:
                    out = out[:i] + c.upper() + out[i + 1:]
                    capitalised = 1
                break
    return out, echoed, strips, capitalised


def split_finals(text):
    """`text` split at sentence ends: one Saaras final per sentence. The
    segmenter must be fed as finals arrive, not the whole text at once — it
    cuts at the *last* sentence end in what it has not handed out yet, so the
    whole text in one go is one enormous chunk and no simulation at all. One
    sentence per final is the finest granularity the service can produce (a
    real final often holds several, which only means fewer, larger chunks and
    fewer background calls, so this is the costly end of the range). The
    trailing fragment, if any, comes last: it is what the app would still be
    holding as a partial when the key goes up."""
    out, start, i, n = [], 0, 0, len(text)
    while i < n:
        c = text[i]
        decimal = (c == "." and 0 < i < n - 1
                   and text[i - 1] in "0123456789" and text[i + 1] in "0123456789")
        if c in TERMINALS and not decimal:
            end = i + 1
            while end < n and text[end] in CLOSERS:
                end += 1
            piece = text[start:end].strip()
            if piece:
                out.append(piece)
            start = i = end
            continue
        i += 1
    rest = text[start:].strip()
    if rest:
        out.append(rest)
    return out


def seam_check(pieces):
    """Anomalies at the joins of the assembled text: a lowercase start after a
    closed sentence, a doubled terminal mark, or a 5-gram of the previous
    chunk's last 40 words echoed in the next chunk's first 40 (the model
    repeating its <before_cursor> context). Returns (joins, anomalous joins,
    per-kind counts)."""
    kinds = {"lowercase_start": 0, "doubled_mark": 0, "echoed_context": 0}
    joins = 0
    anomalies = 0
    for prev, nxt in zip(pieces, pieces[1:]):
        prev, nxt = prev.strip(), nxt.strip()
        if not prev or not nxt:
            continue
        joins += 1
        hit = False
        closed = prev.rstrip(CLOSERS).endswith(tuple(TERMINALS))
        if closed and nxt[0].isalpha() and nxt[0].islower():
            kinds["lowercase_start"] += 1
            hit = True
        if prev[-1] in TERMINALS and nxt[0] in TERMINALS:
            kinds["doubled_mark"] += 1
            hit = True
        # Casing and punctuation are exactly what the model may have changed
        # while echoing, so the 5-grams are compared on bare words.
        before = bare_words(prev)[-40:]
        after = bare_words(nxt)[:40]
        grams = {tuple(before[i:i + 5]) for i in range(max(0, len(before) - 4))}
        if grams and any(tuple(after[i:i + 5]) in grams for i in range(max(0, len(after) - 4))):
            kinds["echoed_context"] += 1
            hit = True
        anomalies += int(hit)
    return joins, anomalies, kinds


async def one_session(audio, cfg, mode, stream_type):
    """Returns (drain_ms, transcript, session_end_seen)."""
    endpointing = "manual" if mode == "manual" else "vad"
    frames = cfg["finish_manual"] if mode == "manual" else cfg["finish_vad"]
    finals = {}
    partial = ""
    got_end = asyncio.Event()
    # Mirrors sarvam::ws: the deadlines arm only once the finish frames are on
    # the wire, so `finish_sent` stands in for the app's `hard_deadline.is_some()`
    # guard on both the partial and the final arms. A final that arrived
    # mid-stream (VAD) must therefore arm nothing.
    finish_sent = False
    quiet_deadline = None
    async with websockets.connect(ws_url(endpointing, stream_type),
                                  additional_headers=realtime_headers(),
                                  max_size=None, open_timeout=10) as ws:
        async def reader():
            nonlocal partial, quiet_deadline
            try:
                async for msg in ws:
                    j = json.loads(msg)
                    typ = j.get("type") or j.get("event")
                    if typ == "transcript.partial":
                        partial = (j.get("data") or {}).get("transcript") or j.get("transcript") or ""
                        # Speech is still being finalized — a live partial
                        # cancels the quiet window; the hard deadline bounds.
                        if finish_sent:
                            quiet_deadline = None
                    elif typ == "transcript.final":
                        d = j.get("data") or j
                        finals[d.get("utterance_idx", len(finals))] = d.get("transcript") or d.get("text") or ""
                        partial = ""
                        if finish_sent:
                            quiet_deadline = time.perf_counter() + cfg["quiet_ms"] / 1000
                    elif typ == "session.end":
                        got_end.set()
                        return
            except websockets.ConnectionClosed:
                return

        rtask = asyncio.create_task(reader())
        await asyncio.sleep(0.3)  # session.begin
        t0 = time.perf_counter()
        if mode == "manual":
            await ws.send(json.dumps({"event": "speech_start"}))
        n = 0
        for off in range(0, len(audio), CHUNK):
            await ws.send(json.dumps({"event": "audio_input",
                                      "audio": base64.b64encode(audio[off:off + CHUNK]).decode()}))
            n += 1
            delay = t0 + n * 0.1 - time.perf_counter()
            if delay > 0:
                await asyncio.sleep(delay)
        t_finish = time.perf_counter()
        for ev in frames:
            await ws.send(json.dumps({"event": ev}))
        finish_sent = True
        # ws.rs arms IDLE_GRACE at the finish frame only when there is nothing
        # in flight: an empty partial and finals already collected.
        if not partial and finals:
            quiet_deadline = t_finish + IDLE_GRACE
        audio_ms = len(audio) / 32
        hard_deadline = t_finish + min(6.0, cfg["hard_floor_ms"] / 1000 + audio_ms / 30 / 1000)
        # Drain-exit rule, as ws.rs: session.end -> exit; else the armed quiet
        # deadline; else the hard deadline.
        while True:
            now = time.perf_counter()
            if got_end.is_set():
                break
            if quiet_deadline is not None and now >= quiet_deadline:
                break
            if now >= hard_deadline:
                break
            await asyncio.sleep(0.005)
        drain_ms = (time.perf_counter() - t_finish) * 1000
        if not got_end.is_set():
            await ws.send(json.dumps({"event": "end"}))  # the goodbye
        rtask.cancel()
    text = " ".join(finals[k] for k in sorted(finals)) or partial
    return drain_ms, text, got_end.is_set()


async def session_or_quota(audio, cfg, mode, stream_type):
    """`one_session`, with the two endings only the relay can produce given
    their names: a `401` on the upgrade is a dead sign-in, and close 4029 with
    reason `quota` is the week's limit (relay/src/user_session.ts). Direct mode
    produces neither, and everything else re-raises untouched."""
    try:
        return await one_session(audio, cfg, mode, stream_type)
    except websockets.InvalidStatus as e:
        if RELAY and e.response.status_code == 401:
            raise TokenRejected() from None
        raise
    except websockets.ConnectionClosed as e:
        if RELAY and getattr(e.rcvd, "code", None) == CLOSE_QUOTA:
            raise QuotaExhausted() from None
        raise


def polish_call(conn, text, cfg, context=None):
    """One polish call in the config's request shape, with the incremental
    <before_cursor> block and its extra rule when `context` is given. Returns a
    dict; its "text" is the reply with the marker removed and is for in-process
    assembly and seam checking only — it is never printed, logged or saved."""
    if cfg["marker"] == "uuid":
        marker = f"__BS_COMPLETE_{uuid.uuid4()}__"
    else:
        marker = "<<" + uuid.uuid4().hex[:4].upper() + ">>"
    context = (context or "").strip()
    system = polish_probe.PROMPTS[
        polish_probe.CONTEXT_PROMPT_NAMES[CONTEXT_RULE] if context else "HIGH(shipped)"]
    user = polish_probe.user_turn(text, cfg["end_marker_rule"].format(m=marker), context)
    payload = polish_probe.body("sarvam-105b", system, user, cfg["stream"], None)
    r = polish_probe.call(conn, payload)
    status = r.get("status")
    if RELAY and status == 401:
        # Every call after this one would be refused for the same reason.
        raise TokenRejected()
    if status != 200:
        # Status code only — `r["error"]` is the service's response body.
        return {"status": status, "ms": None, "ttft_ms": None, "completion_tokens": None,
                "prompt_tokens": None, "marker_ok": None, "text": "", "out_words": None}
    out = r.get("text", "")
    return {"status": status, "ms": r["total_ms"], "ttft_ms": r.get("ttft_ms"),
            "completion_tokens": r.get("completion_tokens"),
            "prompt_tokens": r.get("prompt_tokens"),
            "marker_ok": out.rstrip().endswith(marker),
            "text": out.replace(marker, " ").strip(),
            "out_words": len(out.replace(marker, " ").split())}


def polish(conn, text, cfg):
    """Returns (polish_ms, ttft_ms, completion_tokens, ended_with_marker, status,
    out_words). Only the word count of the reply leaves this function — never
    the reply itself."""
    r = polish_call(conn, text, cfg)
    if r["status"] != 200:
        return None, None, None, None, r["status"], None
    return (r["ms"], r["ttft_ms"], r["completion_tokens"], r["marker_ok"],
            r["status"], r["out_words"])


def polish_on_own_conn(text, cfg):
    """polish() on a private connection, for the burst path: http.client
    connections are not thread-safe, and the burst's to_thread polish calls
    run in parallel."""
    c = chat_conn()
    try:
        return polish(c, text, cfg)
    finally:
        c.close()


def fixture_cases(field="input", lower=False):
    """The fixture's `field`, split into words, in file order. `input` is
    lowercased and unpunctuated — the unformatted condition, kept exactly as
    the fixture gives it. `target` lowercased is the incremental condition:
    punctuated but uncased, which is what Saaras hands the app."""
    cases = []
    with open(FIXTURE, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            value = json.loads(line)[field]
            words = (value.lower() if lower else value).split()
            if words:
                cases.append(words)
    if not cases:
        raise SystemExit(f"no usable cases in {FIXTURE}")
    return cases


def paragraph(cases, start, target):
    """`target` words of unformatted speech: consecutive cases from `start`,
    joined by single spaces, wrapping around the corpus (it is ~3.5k words,
    shorter than a full ladder) and trimmed to exactly `target` so every sample
    in a bucket is the same length."""
    out = []
    i = start
    while len(out) < target:
        out.extend(cases[i % len(cases)])
        i += 1
    return " ".join(out[:target])


def incremental_sample(conn, text, cfg):
    """One dictation polished incrementally: every chunk the segmenter closes
    is polished with the polished text before it as context — in the app that
    happens while the user is still speaking, so it is timed but off the
    critical path — and only the tail is timed as what the user waits for.
    Where the whole text still fits one call, that call runs too, and its
    output is diffed against the assembly. Returns the row: counts,
    timings and ratios only, never text."""
    seg = Segmenter()
    outputs = []  # each chunk's polished text after repair, in order, then the tail's
    raw_outputs = []  # the same pieces as the model returned them, for seams_before_repair
    bg_ms, bg_prompt_tokens, bg_failed, bg_over_6s = [], 0, 0, 0
    bg_completion_tokens, chunk_words, chunk_out_words = 0, [], []
    over_expansion, context_calls, context_echoes = 0, 0, 0
    echoed_words_total, echo_strips, capitalised_joins = 0, 0, 0

    def append(piece, inp):
        """Repair against everything already assembled, then append.
        Records what the repair had to do. `inp` is what the model was
        given for this piece — the app passes the chunk's rule-cleaned text
        there; this harness sends the chunk text itself, so that is its
        equivalent."""
        nonlocal echoed_words_total, echo_strips, capitalised_joins
        raw_outputs.append(piece)
        if REPAIR:
            piece, echoed, strips, capped = repair_seam(assemble_polished(outputs), piece, inp)
            echoed_words_total += echoed
            echo_strips += strips
            capitalised_joins += capped
        outputs.append(piece)

    joined = ""
    for final in split_finals(text):
        joined = f"{joined} {final}".strip()
        while True:
            chunk = seg.take_chunk(joined)
            if chunk is None:
                break
            ctx = context_tail(assemble_polished(outputs))
            r = polish_call(conn, chunk, cfg, context=ctx)
            # A failed chunk keeps its own text, exactly as the app falls back
            # to the rule-cleaned chunk; nothing is ever lost at a seam.
            ok = r["status"] == 200 and r["text"]
            append(r["text"] if ok else chunk, chunk)
            bg_failed += int(not ok)
            chunk_words.append(word_count(chunk))
            chunk_out_words.append(word_count(outputs[-1]))
            # over_expansion and context_echoes judge the model, so they read
            # the reply as it arrived — before the repair cleans up after it.
            over_expansion += int(bool(ok) and over_expanded(chunk, r["text"]))
            context_calls += int(bool(ctx))
            context_echoes += int(bool(ok) and bool(ctx) and echoes_context(ctx, r["text"]))
            if r["ms"] is not None:
                bg_ms.append(r["ms"])
                bg_over_6s += int(r["ms"] > POLISH_TIMEOUT_MS)
            bg_prompt_tokens += r["prompt_tokens"] or 0
            bg_completion_tokens += r["completion_tokens"] or 0
            time.sleep(CHUNK_PACE_S)

    tail = seg.tail(joined)
    tr = None
    if tail:
        ctx = context_tail(assemble_polished(outputs))
        tr = polish_call(conn, tail, cfg, context=ctx)
        tail_ok = tr["status"] == 200 and tr["text"]
        append(tr["text"] if tail_ok else tail, tail)
        over_expansion += int(bool(tail_ok) and over_expanded(tail, tr["text"]))
        context_calls += int(bool(ctx))
        context_echoes += int(bool(tail_ok) and bool(ctx) and echoes_context(ctx, tr["text"]))
    assembled = assemble_polished(outputs)
    # The contracted seam fields are measured on the repaired pieces — what
    # would actually be pasted. The unrepaired count rides alongside so the
    # repair's effect is visible in the same row.
    joins, anomalies, kinds = seam_check(outputs)
    _, anomalies_before, kinds_before = seam_check(raw_outputs)
    out_words = word_count(assembled)
    words = word_count(text)
    critical_ms = None if tr is None else tr["ms"]

    # Whole-text comparison: the same input through one call, where it fits.
    # Twice, because the model is not deterministic even at temperature 0: the
    # difference between two whole-text replies is the floor below which a
    # chunked-vs-whole difference means nothing.
    parity_diff = noise_floor = whole_ms = whole_out_words = whole_status = None
    if words <= MAX_INPUT_WORDS:
        time.sleep(CHUNK_PACE_S)
        wr = polish_call(conn, text, cfg)  # no context: byte-identical to shipped
        whole_ms, whole_status = wr["ms"], wr["status"]
        if wr["status"] == 200 and wr["text"]:
            whole_out_words = wr["out_words"]
            ratio = difflib.SequenceMatcher(None, wr["text"].split(), assembled.split()).ratio()
            parity_diff = round(1 - ratio, 4)
            time.sleep(CHUNK_PACE_S)
            wr2 = polish_call(conn, text, cfg)
            if wr2["status"] == 200 and wr2["text"]:
                nr = difflib.SequenceMatcher(None, wr["text"].split(), wr2["text"].split()).ratio()
                noise_floor = round(1 - nr, 4)

    return {
        "words": words, "background_calls": seg.chunks_taken(),
        "background_ms": [round(m) for m in bg_ms],
        "background_ms_total": round(sum(bg_ms)) if bg_ms else 0,
        "background_failed": bg_failed, "background_over_6s": bg_over_6s,
        "background_completion_tokens": bg_completion_tokens,
        # A chunk whose output is far longer than its input is the model
        # running on rather than formatting — the thing the guardrail rejects.
        "chunk_words": chunk_words, "chunk_out_words": chunk_out_words,
        "critical_ms": critical_ms,
        "tail_words": word_count(tail),
        "ttft_ms": None if tr is None else tr["ttft_ms"],
        "completion_tokens": None if tr is None else tr["completion_tokens"],
        # The incremental path's prompt cost: background chunks + the tail.
        # The whole-text calls are measurement, not something the app sends.
        "prompt_tokens": bg_prompt_tokens + (0 if tr is None else (tr["prompt_tokens"] or 0)),
        "out_words": out_words,
        "ratio": None if not (out_words and words) else round(out_words / words, 3),
        "marker_ok": None if tr is None else tr["marker_ok"],
        "over_6s": None if critical_ms is None else critical_ms > POLISH_TIMEOUT_MS,
        "seam_joins": joins, "seam_anomalies": anomalies, "seam_kinds": kinds,
        "seams_before_repair": anomalies_before, "seam_kinds_before_repair": kinds_before,
        "over_expansion": over_expansion,
        "context_calls": context_calls, "context_echoes": context_echoes,
        # What the deterministic repair did on this sample.
        "echoed_words_total": echoed_words_total, "echo_strips": echo_strips,
        "capitalised_joins": capitalised_joins,
        "parity_diff_ratio": parity_diff, "parity_noise_floor": noise_floor,
        "whole_ms": whole_ms,
        "whole_out_words": whole_out_words, "whole_status": whole_status,
        "polish_status": None if tr is None else tr["status"],
    }


def tag_suffix(args):
    """`--tag` appended to the output filename, so runs that differ only in
    their flags (rule version, repair on or off) do not overwrite each other."""
    tag = (getattr(args, "tag", "") or "").strip()
    return f"_{tag}" if tag else ""


def run_text_only(args):
    """The long-dictation ladder: no WebSocket, just the polish call on
    paragraphs of known length. Sequential, ~1 s apart."""
    global CONTEXT_RULE, REPAIR
    CONTEXT_RULE = args.context_rule
    REPAIR = not args.no_repair
    cfg = CONFIGS[args.config]
    incremental = bool(cfg.get("incremental"))
    # The incremental input is what Saaras hands the app — punctuated,
    # uncased — and its sentence ends are what the segmenter cuts at; the
    # fixture's `input` field has none. Same cases, same offsets, so the two
    # ladders line up.
    cases = fixture_cases("target", lower=True) if incremental else fixture_cases()
    try:
        buckets = [int(w) for w in args.words.split(",") if w.strip()]
    except ValueError:
        raise SystemExit(f"--words must be a comma-separated list of integers: {args.words!r}")
    if not buckets or args.per_bucket < 1:
        raise SystemExit("--words must name at least one bucket and --per-bucket at least one sample")
    rows = []
    conn = chat_conn()
    out = os.path.join(os.getcwd(),
                       f"e2e_stress_textonly_{args.config}{tag_suffix(args)}.json")

    def save(summary):
        """Rewritten after every sample, so a crash at #23 keeps 22 rows."""
        with open(out, "w") as f:
            json.dump({**run_meta(), "summary": summary, "rows": rows}, f, indent=1)

    # Samples start at different offsets so no two paragraphs are the same text:
    # each bucket is phase-shifted, and its samples are spread across the corpus.
    stride = max(1, len(cases) // args.per_bucket)
    if RELAY:
        print(f"transport=relay relay={RELAY} usage={relay_usage()}")
    print(f"config={args.config} stream={int(cfg['stream'])} marker={cfg['marker']} "
          f"incremental={int(incremental)} buckets={buckets} per_bucket={args.per_bucket} "
          f"cases={len(cases)}"
          + (f" chunk_min={CHUNK_MIN_WORDS} chunk_max={MAX_CHUNK_WORDS} "
             f"context_chars={CONTEXT_MAX_CHARS} context_rule={CONTEXT_RULE} "
             f"repair={int(REPAIR)}" if incremental else ""))
    for b, target in enumerate(buckets):
        for k in range(args.per_bucket):
            start = (b * 37 + k * stride) % len(cases)
            text = paragraph(cases, start, target)
            words = len(text.split())
            skipped = words > MAX_INPUT_WORDS
            try:
                if incremental:
                    row = incremental_sample(conn, text, cfg)
                else:
                    polish_ms, ttft, ct, marker_ok, status, out_words = polish(conn, text, cfg)
                    row = {"polish_ms": polish_ms, "ttft_ms": ttft, "completion_tokens": ct,
                           "out_words": out_words,
                           "ratio": None if not (out_words and words) else round(out_words / words, 3),
                           "marker_ok": marker_ok,
                           "over_6s": None if polish_ms is None else polish_ms > POLISH_TIMEOUT_MS,
                           "polish_status": status}
            except (TokenRejected, QuotaExhausted):
                raise  # relay-only, and the run is over either way
            except Exception as e:
                # One bad sample must not cost the run. Type only: the message
                # can carry a URL with the key in it, or transcript text.
                rows.append({"bucket": target, "sample": k + 1, "words": words,
                             "polish_ms": None, "critical_ms": None, "ttft_ms": None,
                             "completion_tokens": None,
                             "out_words": None, "ratio": None, "marker_ok": None,
                             "over_6s": None, "skipped_by_app": skipped,
                             "polish_status": None, "error": type(e).__name__})
                print(f"words={target:4d} #{k + 1} FAILED {type(e).__name__}")
                try:
                    conn.close()  # the connection may be the thing that broke
                except Exception:
                    pass
                conn = chat_conn()
                save(None)
                time.sleep(1.0)
                continue
            row.update(bucket=target, sample=k + 1, words=words, skipped_by_app=skipped)
            rows.append(row)
            status = row["polish_status"]
            if incremental:
                # `polish` here is the critical path only — the tail call, which
                # is all the user waits for once the key is up.
                print(f"words={target:4d} #{k + 1} "
                      f"bg={row['background_calls']}x{row['background_ms_total']:>6} "
                      f"crit={'n/a' if row['critical_ms'] is None else round(row['critical_ms']):>5} "
                      f"ttft={'n/a' if row['ttft_ms'] is None else round(row['ttft_ms']):>5} "
                      f"tail_words={row['tail_words']:>3} "
                      f"out={row['out_words']:>4} "
                      f"ratio={'n/a' if row['ratio'] is None else format(row['ratio'], '.2f')} "
                      f"pt={row['prompt_tokens']:>6} "
                      f"seams={row['seam_anomalies']}/{row['seam_joins']} "
                      f"(pre={row['seams_before_repair']}) "
                      f"echo={row['context_echoes']}/{row['context_calls']} "
                      f"repair={row['echo_strips']}s/{row['echoed_words_total']}w/"
                      f"{row['capitalised_joins']}c "
                      f"parity={'n/a' if row['parity_diff_ratio'] is None else format(row['parity_diff_ratio'], '.3f')} "
                      f"noise={'n/a' if row['parity_noise_floor'] is None else format(row['parity_noise_floor'], '.3f')} "
                      f"over_6s={'n/a' if row['over_6s'] is None else int(row['over_6s'])}"
                      f"{'' if status in (None, 200) else ' status=' + str(status)}")
            else:
                print(f"words={target:4d} #{k + 1} polish={'n/a' if row['polish_ms'] is None else round(row['polish_ms']):>6} "
                      f"ttft={'n/a' if row['ttft_ms'] is None else round(row['ttft_ms']):>5} "
                      f"ct={row['completion_tokens'] if row['completion_tokens'] is not None else 'n/a':>5} "
                      f"out={row['out_words'] if row['out_words'] is not None else 'n/a':>4} "
                      f"ratio={'n/a' if row['ratio'] is None else format(row['ratio'], '.2f')} "
                      f"marker={'n/a' if row['marker_ok'] is None else int(row['marker_ok'])} "
                      f"over_6s={'n/a' if row['over_6s'] is None else int(row['over_6s'])} "
                      f"skipped_by_app={int(skipped)}"
                      f"{'' if status in (None, 200) else ' status=' + str(status)}")
            save(None)
            time.sleep(1.0)

    fmt = lambda v: "n/a" if v is None else "/".join(f"{x:.0f}" for x in v)
    summary = []
    for target in buckets:
        got = [r for r in rows if r["bucket"] == target]
        if incremental:
            ok = [r for r in got if r.get("critical_ms") is not None]
            ratios = [r["ratio"] for r in ok if r["ratio"] is not None]
            calls = [r["background_calls"] for r in got if r.get("background_calls") is not None]
            bg_all = [m for r in got for m in r.get("background_ms", [])]
            bg = percentiles(bg_all)
            parities = [r["parity_diff_ratio"] for r in got if r.get("parity_diff_ratio") is not None]
            noises = [r["parity_noise_floor"] for r in got if r.get("parity_noise_floor") is not None]
            kinds = {k: sum(r.get("seam_kinds", {}).get(k, 0) for r in got)
                     for k in ("lowercase_start", "doubled_mark", "echoed_context")}
            kinds_before = {k: sum(r.get("seam_kinds_before_repair", {}).get(k, 0) for r in got)
                            for k in ("lowercase_start", "doubled_mark", "echoed_context")}
            s = {"bucket": target, "n": len(got), "polished": len(ok),
                 "critical_p50_p90_p99": percentiles([r["critical_ms"] for r in ok]),
                 "ttft_p50_p90_p99": percentiles([r["ttft_ms"] for r in ok if r["ttft_ms"] is not None]),
                 "background_calls": round(sum(calls) / len(calls), 2) if calls else None,
                 "background_p50_p90": None if bg is None else (bg[0], bg[1]),
                 "background_over_6s": sum(r.get("background_over_6s") or 0 for r in got),
                 "background_failed": sum(r.get("background_failed") or 0 for r in got),
                 "prompt_tokens_total": sum(r.get("prompt_tokens") or 0 for r in got),
                 "over_6s_critical": sum(1 for r in ok if r["over_6s"]),
                 "marker_missing": sum(1 for r in got if r.get("marker_ok") is False),
                 "seam_joins": sum(r.get("seam_joins") or 0 for r in got),
                 "seam_anomalies": sum(r.get("seam_anomalies") or 0 for r in got),
                 "seam_kinds": kinds,
                 "seams_before_repair": sum(r.get("seams_before_repair") or 0 for r in got),
                 "seam_kinds_before_repair": kinds_before,
                 "repair_echo_strips": sum(r.get("echo_strips") or 0 for r in got),
                 "repair_echoed_words": sum(r.get("echoed_words_total") or 0 for r in got),
                 "repair_capitalised": sum(r.get("capitalised_joins") or 0 for r in got),
                 "over_expansion": sum(r.get("over_expansion") or 0 for r in got),
                 "context_calls": sum(r.get("context_calls") or 0 for r in got),
                 "context_echoes": sum(r.get("context_echoes") or 0 for r in got),
                 "calls": sum((r.get("background_calls") or 0)
                              + (1 if r.get("critical_ms") is not None else 0) for r in got),
                 "parity_diff_ratio": round(sum(parities) / len(parities), 4) if parities else None,
                 "parity_samples": len(parities),
                 "parity_noise_floor": round(sum(noises) / len(noises), 4) if noises else None,
                 "noise_samples": len(noises),
                 "context_rule": CONTEXT_RULE, "repair": REPAIR,
                 "ratio_min": min(ratios) if ratios else None,
                 "ratio_max": max(ratios) if ratios else None,
                 "errors": sum(1 for r in got if r.get("error"))}
            summary.append(s)
            print(f"bucket={target:4d} n={s['n']} critical p50/p90/p99={fmt(s['critical_p50_p90_p99']):>18} "
                  f"bg calls={s['background_calls']} p50/p90={fmt(s['background_p50_p90']):>12} "
                  f"pt_total={s['prompt_tokens_total']:>7} "
                  f"over_6s_critical={s['over_6s_critical']}/{len(ok)} "
                  f"seams={s['seam_anomalies']}/{s['seam_joins']} "
                  f"pre_repair={s['seams_before_repair']} "
                  f"over_expansion={s['over_expansion']}/{s['calls']} "
                  f"context_echoes={s['context_echoes']}/{s['context_calls']} "
                  f"repair={s['repair_echo_strips']}s/{s['repair_echoed_words']}w/"
                  f"{s['repair_capitalised']}c "
                  f"parity={'n/a' if s['parity_diff_ratio'] is None else format(s['parity_diff_ratio'], '.3f')}"
                  f"(n={s['parity_samples']}) "
                  f"noise={'n/a' if s['parity_noise_floor'] is None else format(s['parity_noise_floor'], '.3f')}"
                  f"(n={s['noise_samples']}) "
                  f"ratio={'n/a' if not ratios else f'{min(ratios):.2f}-{max(ratios):.2f}'} "
                  f"bg_failed={s['background_failed']} errors={s['errors']}")
            continue
        ok = [r for r in got if r["polish_ms"] is not None]
        ratios = [r["ratio"] for r in ok if r["ratio"] is not None]
        s = {"bucket": target, "n": len(got), "polished": len(ok),
             "polish_p50_p90_p99": percentiles([r["polish_ms"] for r in ok]),
             "ttft_p50_p90_p99": percentiles([r["ttft_ms"] for r in ok if r["ttft_ms"] is not None]),
             "over_6s": sum(1 for r in ok if r["over_6s"]),
             "marker_missing": sum(1 for r in got if r["marker_ok"] is False),
             "ratio_min": min(ratios) if ratios else None,
             "ratio_max": max(ratios) if ratios else None,
             "skipped_by_app": target > MAX_INPUT_WORDS,
             "errors": sum(1 for r in got if r.get("error"))}
        summary.append(s)
        p = s["polish_p50_p90_p99"]
        t = s["ttft_p50_p90_p99"]
        print(f"bucket={target:4d} n={s['n']} polish p50/p90/p99={fmt(p):>18} "
              f"ttft p50/p90/p99={fmt(t):>16} over_6s={s['over_6s']}/{len(ok)} "
              f"marker_missing={s['marker_missing']} "
              f"ratio={'n/a' if not ratios else f'{min(ratios):.2f}-{max(ratios):.2f}'} "
              f"skipped_by_app={int(s['skipped_by_app'])} errors={s['errors']}")
    save(summary)
    print("saved", out)


async def run(args):
    cfg = CONFIGS[args.config]
    audio = pcm(os.path.join(HERE, args.wav))
    rows = []
    conn = chat_conn()
    usage_before = relay_usage() if RELAY else None
    if RELAY:
        print(f"transport=relay relay={RELAY} usage={usage_before}")
    out = os.path.join(
        os.getcwd(),
        f"e2e_stress_{args.config}_{args.mode}_c{args.concurrency}{tag_suffix(args)}.json")

    def save(summary):
        """Rewritten after every iteration, so a crash at #45 keeps 44 rows."""
        with open(out, "w") as f:
            json.dump({**run_meta(), "summary": summary, "rows": rows}, f, indent=1)

    async def iteration(i):
        nonlocal conn
        try:
            drain_ms, text, ended = await session_or_quota(audio, cfg, args.mode, args.stream_type)
            words = len(text.split())
            status = None
            if words == 0:
                # Nothing was heard. Polishing "" would measure the model on an
                # empty prompt, which is not a path the app ever takes.
                polish_ms = ttft = ct = marker_ok = None
            elif args.concurrency > 1:
                polish_ms, ttft, ct, marker_ok, status, _ = await asyncio.to_thread(
                    polish_on_own_conn, text, cfg)
            else:
                polish_ms, ttft, ct, marker_ok, status, _ = await asyncio.to_thread(
                    polish, conn, text, cfg)
            row = {"i": i, "drain_ms": drain_ms, "polish_ms": polish_ms, "ttft_ms": ttft,
                   "completion_tokens": ct, "session_end": ended, "marker_ok": marker_ok,
                   "words": words,
                   "total_ms": None if polish_ms is None else drain_ms + polish_ms}
            if status is not None and status != 200:
                row["polish_status"] = status
            rows.append(row)
            print(f"#{i:3d} drain={drain_ms:6.0f} polish={'n/a' if polish_ms is None else round(polish_ms):>5} "
                  f"ttft={'n/a' if ttft is None else round(ttft):>5} total={'n/a' if row['total_ms'] is None else round(row['total_ms']):>5} "
                  f"words={words:3d} session_end={int(ended)} marker={'n/a' if marker_ok is None else int(marker_ok)}"
                  f"{'' if status in (None, 200) else ' status=' + str(status)}")
        except (TokenRejected, QuotaExhausted):
            raise  # relay-only, and the run is over either way
        except Exception as e:
            # One bad iteration must not cost the run. Type only: the message
            # can carry a URL with the key in it, or transcript text.
            rows.append({"i": i, "drain_ms": None, "polish_ms": None, "ttft_ms": None,
                         "completion_tokens": None, "session_end": False, "marker_ok": None,
                         "words": 0, "total_ms": None, "error": type(e).__name__})
            print(f"#{i:3d} FAILED {type(e).__name__}")
            if args.concurrency <= 1:
                # The shared connection may be the thing that broke.
                try:
                    conn.close()
                except Exception:
                    pass
                conn = chat_conn()
        save(None)

    # `quota` is the relay's 4029: the week's words are spent, so there is
    # nothing left to measure. The run stops there and reports what it has —
    # that is the proof the limit bites, not a failure.
    quota = False
    try:
        if args.concurrency <= 1:
            for i in range(1, args.n + 1):
                await iteration(i)
                await asyncio.sleep(0.5)
        else:
            # Bursts: `concurrency` sessions launched together, each polishing on
            # its own chat connection (http.client is not thread-safe, so the
            # parallel to_thread calls cannot share one).
            i = 1
            while i <= args.n:
                batch = [iteration(k) for k in range(i, min(i + args.concurrency, args.n + 1))]
                await asyncio.gather(*batch)
                i += args.concurrency
                await asyncio.sleep(0.5)
    except QuotaExhausted:
        quota = True
        print(f"relay: close {CLOSE_QUOTA}/quota - this week's words are spent, stopping")

    ok = [r for r in rows if r["total_ms"] is not None]
    d = percentiles([r["drain_ms"] for r in rows if r["drain_ms"] is not None])
    p = percentiles([r["polish_ms"] for r in ok])
    t = percentiles([r["total_ms"] for r in ok])
    tt = percentiles([r["ttft_ms"] for r in ok if r["ttft_ms"] is not None])
    misses = sum(1 for r in rows if not r["session_end"])
    # Only rows where a polish actually ran and returned text have a verdict.
    marker_missing = sum(1 for r in rows if r["marker_ok"] is False)
    empty = sum(1 for r in rows if r["words"] == 0)
    errors = sum(1 for r in rows if r.get("error"))
    summary = {"config": args.config, "mode": args.mode, "n": len(rows), "polished": len(ok),
               "drain_p50_p90_p99": d, "polish_p50_p90_p99": p, "ttft_p50_p90_p99": tt,
               "total_p50_p90_p99": t, "session_end_misses": misses,
               "marker_missing": marker_missing, "empty_transcripts": empty,
               "errors": errors,
               "concurrency": args.concurrency, "stream_type": args.stream_type,
               **({"transport": "relay", "relay": RELAY, "quota": quota,
                   "usage_before": usage_before, "usage_after": relay_usage()}
                  if RELAY else {})}
    print(json.dumps(summary, indent=1))
    save(summary)
    print("saved", out)


def dry_run(args):
    """Print the exact URLs and headers this run would use, and connect to
    nothing — so the app's own log lines can be eyeballed against them before
    any call is spent. The token is never printed: only that one was loaded,
    and how long it is."""
    cfg = CONFIGS[args.config or "shipped"]
    endpointing = "manual" if args.mode == "manual" else "vad"
    accept = "text/event-stream" if cfg["stream"] else "application/json"
    print(f"transport   {'relay' if RELAY else 'direct (Sarvam)'}")
    if RELAY:
        print(f"relay       {RELAY}")
        print(f"token       {args.token_file} ({len(TOKEN)} chars, never printed)")
        print(f"realtime    GET {ws_url(endpointing, args.stream_type)}")
        print("            authorization: Bearer <access token>")
        print("            (no api-subscription-key: this lane has no Sarvam key)")
        print(f"chat        POST {RELAY}{RELAY_CHAT_PATH}")
        print("            authorization: Bearer <access token>")
        print("            content-type: application/json")
        print(f"            accept: {accept}")
        print(f"usage       GET {RELAY}{RELAY_USAGE_PATH}")
        print("            authorization: Bearer <access token>")
    else:
        print(f"realtime    GET {ws_url(endpointing, args.stream_type)}")
        print("            api-subscription-key: <SARVAM_API_KEY>")
        print(f"chat        POST https://{CHAT_HOST}{CHAT_PATH}")
        print("            api-subscription-key: <SARVAM_API_KEY>")
        print("            content-type: application/json")
        print(f"            accept: {accept}")
    print(f"body        model=sarvam-105b temperature=0.0 max_tokens=2048 "
          f"reasoning_effort=null stream={int(cfg['stream'])}"
          + (" stream_options.include_usage=1" if cfg["stream"] else ""))
    print(f"run         config={args.config} mode={args.mode} n={args.n} "
          f"stream_type={args.stream_type} marker={cfg['marker']} "
          f"text_only={int(bool(args.text_only))}")
    print("dry run: nothing was connected to")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", choices=CONFIGS)
    ap.add_argument("--mode", choices=["manual", "vad"], default="manual")
    ap.add_argument("--n", type=int, default=50)
    ap.add_argument("--concurrency", type=int, default=1)
    ap.add_argument("--stream-type", default="balanced")
    ap.add_argument("--wav", default="utt_short.wav")
    ap.add_argument("--text-only", action="store_true",
                    help="skip the WebSocket; polish long unformatted paragraphs instead")
    ap.add_argument("--words", default="100,200,300,400,500,600",
                    help="--text-only: comma-separated input lengths, in words")
    ap.add_argument("--per-bucket", type=int, default=5,
                    help="--text-only: distinct paragraphs per length")
    ap.add_argument("--context-rule", choices=["v1", "v2"], default="v2",
                    help="incremental: which <before_cursor> rule the context calls carry "
                         "(v2 ships; v1 is an earlier wording, for comparison)")
    ap.add_argument("--no-repair", action="store_true",
                    help="incremental: skip the deterministic seam repair, so the seam numbers "
                         "measure the model alone")
    ap.add_argument("--tag", default="",
                    help="appended to the output filename, to keep runs apart")
    ap.add_argument("--relay", default="",
                    help="Cloud mode: the relay's base URL (https://..., or "
                         "http://127.0.0.1:8787 for a local `wrangler dev`). "
                         "Needs --token-file; no Sarvam key is read or required")
    ap.add_argument("--token-file", default="",
                    help="Cloud mode: the file cloud_token.py wrote the "
                         "Supabase access token to")
    ap.add_argument("--dry-run", action="store_true",
                    help="print the URLs and headers this run would use and "
                         "connect to nothing")
    a = ap.parse_args()
    # `--relay` was read out of argv before `polish_probe` was imported;
    # argparse accepts abbreviations and this did not, so make the two agree
    # rather than running half in one lane and half in the other.
    if (a.relay or "").rstrip("/") != RELAY:
        ap.error("spell --relay in full: it is read before the argument parser runs")
    if a.relay and not a.token_file:
        ap.error("--relay needs --token-file (write one with cloud_token.py)")
    if a.token_file and not a.relay:
        ap.error("--token-file is Cloud mode only; it needs --relay")
    if RELAY:
        TOKEN = load_token(a.token_file)
    if a.dry_run:
        dry_run(a)
        sys.exit(0)
    try:
        if a.text_only:
            # The ladder has one shipped answer to compare against, so it defaults.
            a.config = a.config or "shipped"
            run_text_only(a)
        else:
            if not a.config:
                ap.error("--config is required")  # never guess which arm a gate run meant
            asyncio.run(run(a))
    except TokenRejected:
        # Said once, and the run stops: every remaining call would be refused
        # for the same reason, and a token lasts about an hour.
        sys.exit("token rejected - rerun cloud_token.py")
