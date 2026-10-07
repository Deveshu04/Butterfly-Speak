"""Saaras realtime finalisation probe.

Streams a 16 kHz mono WAV in real time over the app's own WebSocket protocol and
timestamps every server frame relative to the moment the client sends its
finish frame(s). Answers: how long after speech_end does the final arrive, does
`end` (vs `flush`) produce a prompt session.end, do finals arrive mid-stream
under manual endpointing, and how close is the last partial to the final.

Needs `websockets` (pip) and two WAVs next to this file, `utt_short.wav`
(~4 s, one sentence) and `utt_long.wav` (~35 s with two 1.5 s pauses);
`make_fixtures.ps1` in this directory regenerates both with Windows TTS.

Env: SARVAM_API_KEY (required), BS_STREAM_TYPE (balanced|fast; the app ships
balanced), BS_LANG, BS_MODE, BS_RT_MODEL. Args: scenario letters, e.g.
`python ws_probe.py A B E F`.
"""
import asyncio
import base64
import json
import os
import sys
import time
import wave

import websockets

KEY = os.environ["SARVAM_API_KEY"].strip()
BASE = "wss://api.sarvam.ai/speech-to-text-realtime/ws"
MODEL = os.environ.get("BS_RT_MODEL", "saaras:v3-realtime")
STREAM_TYPE = os.environ.get("BS_STREAM_TYPE", "fast")
MODE = os.environ.get("BS_MODE", "transcribe")
LANG = os.environ.get("BS_LANG", "en-IN")
HERE = os.path.dirname(os.path.abspath(__file__))
CHUNK = 3200  # 100 ms of PCM16 @ 16 kHz


def pcm(path):
    with wave.open(path, "rb") as w:
        assert w.getframerate() == 16000 and w.getnchannels() == 1 and w.getsampwidth() == 2, w.getparams()
        return w.readframes(w.getnframes())


def url(endpointing):
    return (
        f"{BASE}?model={MODEL}&language_code={LANG}&stream_type={STREAM_TYPE}&mode={MODE}"
        f"&endpointing={endpointing}&encoding=linear16&sample_rate=16000"
    )


async def scenario(name, wav, endpointing, finish_frames, post_wait, then_end_after=None, speech_frames=True):
    """finish_frames: list of event names sent back-to-back at end of audio.
    post_wait: seconds to keep listening after the finish frames.
    then_end_after: if set, send {"event":"end"} after that many seconds and keep listening 3 s more."""
    audio = pcm(wav)
    events = []
    t0 = None
    t_finish = None
    last_partial = ""
    finals = {}
    session_end_at = None
    first_final_after_finish = None

    async with websockets.connect(
        url(endpointing), additional_headers={"api-subscription-key": KEY}, max_size=None, open_timeout=10
    ) as ws:
        t_conn = time.perf_counter()

        async def reader():
            nonlocal last_partial, session_end_at, first_final_after_finish
            try:
                async for msg in ws:
                    now = time.perf_counter()
                    try:
                        j = json.loads(msg)
                    except Exception:
                        events.append((now, "raw", str(msg)[:80]))
                        continue
                    typ = j.get("type") or j.get("event") or "?"
                    rel_start = (now - t0) if t0 else 0.0
                    rel_fin = (now - t_finish) if t_finish else None
                    if typ == "transcript.partial":
                        txt = (j.get("data") or {}).get("transcript") or j.get("transcript") or j.get("text") or ""
                        last_partial = txt
                        events.append((now, typ, f"{len(txt.split())}w"))
                    elif typ == "transcript.final":
                        d = j.get("data") or j
                        txt = d.get("transcript") or d.get("text") or ""
                        idx = d.get("utterance_idx", len(finals))
                        finals[idx] = txt
                        if t_finish and first_final_after_finish is None:
                            first_final_after_finish = now
                        events.append((now, typ, f"idx={idx} {len(txt.split())}w: {txt[:70]!r}"))
                    elif typ == "session.end":
                        session_end_at = now
                        events.append((now, typ, ""))
                        break
                    elif typ == "error":
                        events.append((now, typ, json.dumps(j)[:200]))
                    else:
                        events.append((now, typ, json.dumps(j)[:120]))
            except websockets.ConnectionClosed as e:
                events.append((time.perf_counter(), "closed", f"{e.code} {e.reason!r}"))

        rtask = asyncio.create_task(reader())

        # wait for session.begin (up to 3 s), then stream in real time
        await asyncio.sleep(0.3)
        t0 = time.perf_counter()
        if speech_frames:
            await ws.send(json.dumps({"event": "speech_start"}))
        n = 0
        for off in range(0, len(audio), CHUNK):
            await ws.send(json.dumps({"event": "audio_input", "audio": base64.b64encode(audio[off:off + CHUNK]).decode()}))
            n += 1
            target = t0 + n * 0.1
            delay = target - time.perf_counter()
            if delay > 0:
                await asyncio.sleep(delay)
        partial_at_finish = last_partial
        t_finish = time.perf_counter()
        for ev in finish_frames:
            await ws.send(json.dumps({"event": ev}))
        events.append((t_finish, "CLIENT", "+".join(finish_frames)))
        try:
            await asyncio.wait_for(asyncio.shield(rtask), timeout=post_wait)
        except asyncio.TimeoutError:
            pass
        if then_end_after is not None and not rtask.done():
            t_end = time.perf_counter()
            await ws.send(json.dumps({"event": "end"}))
            events.append((t_end, "CLIENT", "end"))
            try:
                await asyncio.wait_for(asyncio.shield(rtask), timeout=3.0)
            except asyncio.TimeoutError:
                pass
        if not rtask.done():
            rtask.cancel()

    # report
    print(f"\n=== {name}  (endpointing={endpointing}, audio={len(audio)/32000:.1f}s, finish={'+'.join(finish_frames)})")
    print(f"  connect->first frame: {(events[0][0]-t_conn)*1000:.0f} ms" if events else "  no frames")
    for t, typ, info in events:
        rel = (t - t_finish) * 1000 if t_finish else (t - t0) * 1000
        tag = "after-finish" if t_finish and t >= t_finish else "during-audio"
        print(f"  {tag:<12} {rel:+8.0f} ms  {typ:<20} {info}")
    final_text = " ".join(finals[k] for k in sorted(finals))
    print(f"  finals={len(finals)}  first-final-after-finish={None if first_final_after_finish is None else round((first_final_after_finish-t_finish)*1000)} ms"
          f"  session.end={None if session_end_at is None else round((session_end_at-t_finish)*1000)} ms")
    print(f"  last partial at finish ({len(partial_at_finish.split())}w): {partial_at_finish[:120]!r}")
    print(f"  assembled finals      ({len(final_text.split())}w): {final_text[:120]!r}")
    print(f"  partial==final(after normalising case/punct): {norm(partial_at_finish)==norm(final_text)}")


def norm(s):
    return "".join(c.lower() for c in s if c.isalnum() or c.isspace()).split()


async def main():
    short = os.path.join(HERE, "utt_short.wav")
    long_ = os.path.join(HERE, "utt_long.wav")
    which = sys.argv[1:] or ["A", "B", "C", "D", "E", "F"]
    if "A" in which:
        await scenario("A manual: speech_end+flush (older app), then end after 5 s", short, "manual", ["speech_end", "flush"], 5.0, then_end_after=0)
    if "B" in which:
        await scenario("B manual: speech_end+end", short, "manual", ["speech_end", "end"], 6.0)
    if "C" in which:
        await scenario("C manual: speech_end+flush+end", short, "manual", ["speech_end", "flush", "end"], 6.0)
    if "D" in which:
        await scenario("D manual long w/ pauses: speech_end+end", long_, "manual", ["speech_end", "end"], 8.0)
    if "E" in which:
        await scenario("E vad long: end only", long_, "vad", ["end"], 8.0, speech_frames=False)
    if "F" in which:
        await scenario("F vad long: flush (older app), then end after 5 s", long_, "vad", ["flush"], 5.0, then_end_after=0, speech_frames=False)


asyncio.run(main())
