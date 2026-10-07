"""Live latency probe for the Butterfly Speak polish call against sarvam-105b.

Reproduces the app's exact request shape (format::backend::build_request_body +
sarvam::chat::build_polish_messages) and splits each call into connection /
time-to-first-token / decode, across three prompt sizes and three utterance
lengths, plus the cost of the end marker and of leaving reasoning on.
Reads SARVAM_API_KEY from the environment. Writes polish_probe_results.json
into the working directory. About 30 calls, paced ~1 s apart, ~2 minutes.
"""
import http.client
import json
import os
import re
import ssl
import sys
import time
import uuid

KEY = os.environ.get("SARVAM_API_KEY", "").strip()
if not KEY:
    sys.exit("SARVAM_API_KEY not set")

HOST = "api.sarvam.ai"
PATH = "/v1/chat/completions"
# The shipped High prompt is read straight out of the source so the probe can
# never drift from what the app sends.
LEVEL_RS = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "..", "src-tauri", "src", "format", "level.rs"
)
OUT = os.path.join(os.getcwd(), "polish_probe_results.json")

# --- prompt fragments, read from format/level.rs and sarvam/chat.rs ---
src = open(LEVEL_RS, encoding="utf-8").read()
m = re.search(r'const RULES_POST_PROCESSOR: &str = r##"(.*?)"##;', src, re.S)
RULES_POST_PROCESSOR = m.group(1)

CHAT_RS = os.path.join(os.path.dirname(LEVEL_RS), "..", "sarvam", "chat.rs")
chat_src = open(CHAT_RS, encoding="utf-8").read()

RUST_ESCAPES = {"n": "\n", "t": "\t", '"': '"', "'": "'", "\\": "\\"}


def rust_str(source, name):
    """The value of `const <name>: &str = ...;` in Rust `source`, as the
    compiler reads it: a raw string exactly as written, a plain string with
    its escapes and line continuations decoded."""
    found = re.search(r"const " + re.escape(name) + r': &str =\s*(r#*)?"', source)
    if not found:
        sys.exit(f"polish_probe: no `const {name}: &str` in the Rust source")
    i = found.end()
    if found.group(1):
        return source[i:source.index('"' + found.group(1)[1:], i)]
    out = []
    while source[i] != '"':
        if source[i] != "\\":
            out.append(source[i])
            i += 1
        elif source[i + 1] == "\n":
            # A backslash at the end of a line drops the break and the next
            # line's leading whitespace.
            i += 2
            while source[i] in " \t\n":
                i += 1
        elif source[i + 1] in RUST_ESCAPES:
            out.append(RUST_ESCAPES[source[i + 1]])
            i += 2
        else:
            sys.exit(f"polish_probe: unsupported escape in {name}: \\{source[i + 1]}")
    return "".join(out)


RULES_COMMON = rust_str(src, "RULES_COMMON")
REMOVE_FILLERS = rust_str(src, "REMOVE_FILLERS")
APPLY_CORRECTIONS = rust_str(src, "APPLY_CORRECTIONS")
HARD_FULL = rust_str(src, "INJECTION_HARDENING_FULL")
HARD_FILLERS = rust_str(src, "INJECTION_HARDENING_FILLERS")
DELIM = rust_str(chat_src, "TRANSCRIPT_DELIMITER_RULE")
SPEECH_OPEN = rust_str(chat_src, "SPEECH_OPEN")
SPEECH_CLOSE = rust_str(chat_src, "SPEECH_CLOSE")
REPLY_CONTRACT = rust_str(chat_src, "POLISH_REPLY_CONTRACT")
# format::backend::end_marker_rule, word for word; `{m}` is the marker.
END_MARKER_RULE = (
    "Finish your text with its own final punctuation. Then write this end marker on a new "
    "line by itself, copied exactly, and stop there; the app deletes that line before anyone "
    "reads your reply:\n{m}")
# Verbatim from sarvam::chat::BEFORE_CURSOR_RULE: spliced in right after the
# delimiter rule and only when a call carries text before the cursor, so a
# call without context is byte-identical to the shipped prompt.
#
# v1 is an earlier wording: it produced a lowercase sentence start at 25 of 36
# anomalous joins and an echo of the context in 21 % of context-carrying
# calls, because "continues it naturally" reads as "continue this sentence".
# Kept so the two rules can still be compared head to head
# (`e2e_stress.py --context-rule v1`).
BEFORE_CURSOR_RULE_V1 = "\n- The text already in the document immediately before the cursor is given in <before_cursor></before_cursor> tags: do not repeat it and do not edit it; format only the transcript so that it continues it naturally (capitalisation, punctuation, list numbering)."
# v2 is what ships: it forbids the echo outright and states that the
# transcript begins a new sentence.
BEFORE_CURSOR_RULE_V2 = "\n- The text already in the document immediately before the cursor is given in <before_cursor></before_cursor> tags. Never output any of that text again: it is already written. The transcript is what comes next after it; it always begins a new sentence, so capitalise its first word and continue any list numbering from where the document left off."
BEFORE_CURSOR_RULE = BEFORE_CURSOR_RULE_V2

PROMPTS = {
    "HIGH(shipped)": RULES_POST_PROCESSOR + DELIM + HARD_FULL,
    "HIGH(shipped)+context": RULES_POST_PROCESSOR + DELIM + BEFORE_CURSOR_RULE_V2 + HARD_FULL,
    "HIGH(shipped)+context-v1": RULES_POST_PROCESSOR + DELIM + BEFORE_CURSOR_RULE_V1 + HARD_FULL,
    "LOCALHIGH(compact)": RULES_COMMON + REMOVE_FILLERS + APPLY_CORRECTIONS + DELIM + HARD_FULL,
    "BALANCED": RULES_COMMON + REMOVE_FILLERS + DELIM + HARD_FILLERS,
}
# Which prompt a context-carrying call uses, by rule version.
CONTEXT_PROMPT_NAMES = {"v1": "HIGH(shipped)+context-v1", "v2": "HIGH(shipped)+context"}

# Saaras-style input: already punctuated, fillers and self-corrections intact.
INPUTS = {
    "S15": "Um, so could you move the standup to half past ten on Thursday? Uh, thanks.",
    "M65": (
        "This is a fully working Butterfly speech dictation model. The only thing which is kind of an "
        "issue right now is the latency on how much time it is taking to polish the text. Um, so what we "
        "need to do is make the latency comparable to plain typing, no wait, plain dictation, where the phrasing "
        "is very quick and everything is at a speed where it is comparable or even better."
    ),
    "L150": (
        "Okay so for tomorrow's meeting there are three things we need to cover. First the budget for Q3, "
        "second the hiring plan for the Bangalore office, and third the, uh, the vendor contracts that are "
        "expiring in October. Um, I also wanted to mention that Priya sent over the revised numbers last "
        "night and they look, uh, they look about twenty percent higher than what we had estimated, so we "
        "should probably, no, we should definitely flag that to finance before the call. Also can someone "
        "book the big conference room, the one on the fourth floor, not the third floor, for two thirty "
        "to four? And, uh, please loop in Rahul from legal because the vendor stuff is going to need his "
        "sign off anyway. I think that's, um, that's everything for now, let me know if I missed anything. "
        "Thanks everyone."
    ),
}


MARKER_STYLE = {"style": "uuid"}


def make_marker():
    style = MARKER_STYLE["style"]
    if style == "uuid":
        return f"__BS_COMPLETE_{uuid.uuid4()}__"
    if style == "short":
        return "<<" + uuid.uuid4().hex[:4].upper() + ">>"
    return ""


def messages(prompt, text, context=None):
    """The app's user turn. `context` is the already-polished text
    immediately before the cursor: it rides in its own block first, exactly as
    sarvam::chat::build_polish_messages writes it, and callers that pass it are
    expected to pass PROMPTS["HIGH(shipped)+context"] as the prompt."""
    marker = make_marker()
    instruction = END_MARKER_RULE.format(m=marker) if marker else ""
    return prompt, user_turn(text, instruction, context), marker


def user_turn(text, instruction, context=None):
    """The polish user turn as sarvam::chat::build_polish_messages writes it:
    the before-cursor block when there is context, the dictation between its
    tags, a blank line, the reply contract, then the end-marker rule (left
    off when there is none). Blank context counts as none."""
    context = (context or "").strip()
    before = f"<before_cursor>\n{context}\n</before_cursor>\n" if context else ""
    ending = f"{REPLY_CONTRACT} {instruction}" if instruction else REPLY_CONTRACT
    return f"{before}{SPEECH_OPEN}\n{text}\n{SPEECH_CLOSE}\n\n{ending}"


def body(model, system, user, stream, reasoning=None, include_usage=True):
    b = {
        "model": model,
        "temperature": 0.0,
        "max_tokens": 2048,
        "reasoning_effort": reasoning,
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
    }
    if stream:
        b["stream"] = True
        if include_usage:
            b["stream_options"] = {"include_usage": True}
    return b


def connect():
    t0 = time.perf_counter()
    conn = http.client.HTTPSConnection(HOST, 443, timeout=60, context=ssl.create_default_context())
    conn.connect()
    return conn, (time.perf_counter() - t0) * 1000


def call(conn, payload):
    data = json.dumps(payload).encode("utf-8")
    hdrs = {
        "api-subscription-key": KEY,
        "Content-Type": "application/json",
        "Accept": "text/event-stream" if payload.get("stream") else "application/json",
    }
    t0 = time.perf_counter()
    conn.request("POST", PATH, body=data, headers=hdrs)
    resp = conn.getresponse()
    t_headers = (time.perf_counter() - t0) * 1000
    status = resp.status
    if status != 200:
        raw = resp.read().decode("utf-8", "replace")
        return {"status": status, "error": raw[:300], "t_headers_ms": t_headers}
    if not payload.get("stream"):
        raw = resp.read().decode("utf-8")
        t_total = (time.perf_counter() - t0) * 1000
        j = json.loads(raw)
        ch = j["choices"][0]
        return {
            "status": 200,
            "t_headers_ms": t_headers,
            "ttft_ms": None,
            "total_ms": t_total,
            "prompt_tokens": j.get("usage", {}).get("prompt_tokens"),
            "completion_tokens": j.get("usage", {}).get("completion_tokens"),
            "usage_raw": j.get("usage"),
            "finish": ch.get("finish_reason"),
            "text": ch["message"]["content"],
        }
    # streaming: parse SSE lines
    ttft = None
    chunks = 0
    text = []
    usage = None
    finish = None
    buf = b""
    while True:
        line = resp.readline()
        if not line:
            break
        line = line.strip()
        if not line.startswith(b"data:"):
            continue
        payload_s = line[5:].strip()
        if payload_s == b"[DONE]":
            # Drain the chunked terminator so the keep-alive connection is reusable.
            try:
                resp.read()
            except Exception:
                pass
            break
        try:
            j = json.loads(payload_s)
        except Exception:
            continue
        if j.get("usage"):
            usage = j["usage"]
        for c in j.get("choices", []):
            d = c.get("delta", {})
            content = d.get("content")
            if content:
                if ttft is None:
                    ttft = (time.perf_counter() - t0) * 1000
                chunks += 1
                text.append(content)
            if c.get("finish_reason"):
                finish = c["finish_reason"]
    t_total = (time.perf_counter() - t0) * 1000
    return {
        "status": 200,
        "t_headers_ms": t_headers,
        "ttft_ms": ttft,
        "total_ms": t_total,
        "chunks": chunks,
        "prompt_tokens": (usage or {}).get("prompt_tokens"),
        "completion_tokens": (usage or {}).get("completion_tokens"),
        "usage_raw": usage,
        "finish": finish,
        "text": "".join(text),
    }


def run():
    results = []
    conn, c_ms = connect()
    print(f"initial connect (DNS+TCP+TLS): {c_ms:.0f} ms")

    def one(label, prompt_name, input_name, stream, reasoning=None, fresh=False, show=False):
        nonlocal conn
        system, user, marker = messages(PROMPTS[prompt_name], INPUTS[input_name])
        connect_ms = 0.0
        if fresh:
            try:
                conn.close()
            except Exception:
                pass
            conn, connect_ms = connect()
        payload = body("sarvam-105b", system, user, stream, reasoning)
        r = call(conn, payload)
        if r.get("status") == 400 and stream and "stream_options" in payload:
            payload.pop("stream_options")
            time.sleep(1.0)
            r = call(conn, payload)
            r["note"] = "stream_options rejected; retried without"
        r.update(
            label=label, prompt=prompt_name, input=input_name, stream=stream,
            reasoning=reasoning, fresh_connection=fresh, connect_ms=connect_ms,
            prompt_chars=len(system), input_words=len(INPUTS[input_name].split()),
        )
        txt = r.get("text", "")
        r["marker_style"] = MARKER_STYLE["style"]
        r["ended_with_marker"] = bool(marker) and txt.rstrip().endswith(marker)
        r["text"] = (txt.replace(marker, "") if marker else txt).strip()
        r["out_words"] = len(r["text"].split())
        results.append(r)
        ct = r.get("completion_tokens")
        ttft = r.get("ttft_ms")
        dec = ""
        if ct and ttft and r.get("total_ms"):
            dec = f" decode={(ct - 1) / max(1e-3, (r['total_ms'] - ttft)) * 1000:.0f} tok/s"
        ttft_s = "    -" if ttft is None else f"{ttft:5.0f}"
        print(
            f"{label:<34} {prompt_name:<20} {input_name:<5} stream={int(stream)} fresh={int(fresh)} "
            f"conn={connect_ms:4.0f} hdr={r.get('t_headers_ms', 0) or 0:5.0f} ttft={ttft_s} "
            f"total={r.get('total_ms', 0) or 0:5.0f} pt={r.get('prompt_tokens')} ct={ct} fin={r.get('finish')} "
            f"marker={int(r['ended_with_marker'])}{dec}"
            + (f" ERR {r.get('status')} {r.get('error')}" if r.get("status") != 200 else "")
        )
        if show:
            print("   OUT:", r["text"][:600].replace("\n", " | "))
        time.sleep(1.1)

    # 1. The shipped path, non-streaming, as the app sends it — 2 reps on M65
    #    (three earlier reps measured 1472/1349/1568 ms, ct 113/115/110).
    for i in range(2):
        one(f"shipped non-stream #{i+1}", "HIGH(shipped)", "M65", stream=False, show=(i == 0))
    # 2. Streaming split for every prompt x input, 2 reps.
    for pn in ["HIGH(shipped)", "LOCALHIGH(compact)", "BALANCED"]:
        for inp in ["S15", "M65", "L150"]:
            for i in range(2):
                one(f"stream {pn[:9]} {inp} #{i+1}", pn, inp, stream=True, show=(i == 0 and inp == "M65"))
    # 3. Marker cost: short marker and no marker, shipped prompt, S15 + M65.
    for style in ["short", "none"]:
        MARKER_STYLE["style"] = style
        for inp in ["S15", "M65"]:
            for i in range(2):
                one(f"marker={style} {inp} #{i+1}", "HIGH(shipped)", inp, stream=True, show=(i == 0 and inp == "M65"))
    MARKER_STYLE["style"] = "uuid"
    # 4. Cold connection cost, shipped prompt, M65, 2 reps.
    for i in range(2):
        one(f"cold-conn shipped #{i+1}", "HIGH(shipped)", "M65", stream=True, fresh=True)
    # 5. What thinking costs if reasoning is NOT disabled (one rep each).
    one("reasoning=low shipped", "HIGH(shipped)", "M65", stream=True, reasoning="low")
    one("reasoning omitted shipped", "HIGH(shipped)", "M65", stream=True, reasoning="__omit__")

    json.dump(results, open(OUT, "w", encoding="utf-8"), indent=1, ensure_ascii=False)
    print("saved", OUT)


if __name__ == "__main__":
    # "__omit__" means: do not send the key at all (server default).
    _orig_body = body

    def body(model, system, user, stream, reasoning=None, include_usage=True):  # noqa: F811
        b = _orig_body(model, system, user, stream, reasoning, include_usage)
        if reasoning == "__omit__":
            b.pop("reasoning_effort", None)
        return b

    run()
