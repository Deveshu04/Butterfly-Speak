import { describe, expect, it } from "vitest";
import { shapeChatBody, shapeClientFrame, shapeRealtimeQuery } from "../src/shape";

describe("shapeRealtimeQuery", () => {
  it("keeps only the allow-listed params and pins the rest", () => {
    const shaped = shapeRealtimeQuery(
      new URL(
        "https://r/v1/realtime?model=evil&language_code=hi-IN&stream_type=balanced&mode=transcribe&endpointing=manual&encoding=x&sample_rate=8000&prompt=Priya%20Rahul&extra=1",
      ),
    );
    expect(shaped).not.toBeNull();
    const q = new URLSearchParams(shaped!);
    expect(q!.get("model")).toBe("saaras:v3-realtime");
    expect(q!.get("language_code")).toBe("hi-IN");
    expect(q!.get("stream_type")).toBe("balanced");
    expect(q!.get("mode")).toBe("transcribe");
    expect(q!.get("endpointing")).toBe("manual");
    expect(q!.get("encoding")).toBe("linear16");
    expect(q!.get("sample_rate")).toBe("16000");
    expect(q!.get("prompt")).toBe("Priya Rahul");
    expect(q!.has("extra")).toBe(false);
  });

  it("rejects values outside the enums", () => {
    expect(
      shapeRealtimeQuery(new URL("https://r/v1/realtime?language_code=en-IN&stream_type=turbo&mode=transcribe&endpointing=manual")),
    ).toBeNull();
    expect(
      shapeRealtimeQuery(new URL("https://r/v1/realtime?language_code=../x&stream_type=balanced&mode=transcribe&endpointing=vad")),
    ).toBeNull();
  });

  it("accepts auto and every app mode, and drops an over-long prompt", () => {
    expect(
      new URLSearchParams(
        shapeRealtimeQuery(new URL("https://r/v1/realtime?language_code=auto&stream_type=fast&mode=translit&endpointing=vad"))!,
      ).get("language_code"),
    ).toBe("auto");
    const long = "x".repeat(2001);
    expect(
      shapeRealtimeQuery(new URL(`https://r/v1/realtime?language_code=en-IN&stream_type=fast&mode=transcribe&endpointing=vad&prompt=${long}`)),
    ).toBeNull();
  });

  it("percent-encodes each value the way the app does: %20 for a space, never +", () => {
    // `sarvam::codec::ws_url` encodes the prompt with `%20`; a `+` is only a
    // space to a form decoder, and Sarvam's query is not a form.
    const shaped = shapeRealtimeQuery(
      new URL(
        "https://r/v1/realtime?model=saaras:v3-realtime&language_code=hi-IN&stream_type=balanced&mode=transcribe&endpointing=manual" +
          "&encoding=linear16&sample_rate=16000&prompt=Priya%20Sharma%2C%20%E0%A4%A8%E0%A4%AE%E0%A4%B8%E0%A5%8D%E0%A4%A4%E0%A5%87",
      ),
    );
    expect(String(shaped)).toBe(
      "model=saaras%3Av3-realtime&language_code=hi-IN&stream_type=balanced&mode=transcribe&endpointing=manual" +
        "&encoding=linear16&sample_rate=16000&prompt=Priya%20Sharma%2C%20%E0%A4%A8%E0%A4%AE%E0%A4%B8%E0%A5%8D%E0%A4%A4%E0%A5%87",
    );
  });

  it("carries no prompt param when there is no prompt", () => {
    expect(
      String(shapeRealtimeQuery(new URL("https://r/v1/realtime?language_code=en-IN&stream_type=fast&mode=transcribe&endpointing=vad"))),
    ).toBe("model=saaras%3Av3-realtime&language_code=en-IN&stream_type=fast&mode=transcribe&endpointing=vad&encoding=linear16&sample_rate=16000");
  });
});

describe("shapeChatBody", () => {
  const ok = {
    model: "sarvam-105b",
    temperature: 0,
    max_tokens: 2048,
    reasoning_effort: null,
    stream: true,
    stream_options: { include_usage: true },
    messages: [
      { role: "system", content: "s" },
      { role: "user", content: "u" },
    ],
  };

  it("accepts the app's exact body", () => {
    const r = shapeChatBody(ok);
    expect(r.ok).toBe(true);
    expect(JSON.parse((r as { ok: true; body: string }).body)).toEqual(ok);
  });

  it("rejects a foreign model, an oversized max_tokens, unknown keys and non-objects", () => {
    expect(shapeChatBody({ ...ok, model: "gpt-4o" })).toMatchObject({ ok: false, status: 400 });
    expect(shapeChatBody({ ...ok, max_tokens: 100000 })).toMatchObject({ ok: false, status: 400 });
    expect(shapeChatBody({ ...ok, tools: [] })).toMatchObject({ ok: false, status: 400 });
    expect(shapeChatBody("x")).toMatchObject({ ok: false, status: 400 });
  });

  it("rejects a body over 128 KB with 413", () => {
    const big = { ...ok, messages: [{ role: "user", content: "x".repeat(130 * 1024) }] };
    expect(shapeChatBody(big)).toMatchObject({ ok: false, status: 413 });
  });

  it("measures the cap in bytes, not UTF-16 code units", () => {
    // 50,000 Devanagari characters: 50,000 code units but three bytes each.
    const hindi = { ...ok, messages: [{ role: "user", content: "क".repeat(50_000) }] };
    expect(shapeChatBody(hindi)).toMatchObject({ ok: false, status: 413 });
  });

  it("accepts a ~90 KB Devanagari note", () => {
    const note = { ...ok, messages: [{ role: "user", content: "क".repeat(30_000) }] };
    expect(shapeChatBody(note)).toMatchObject({ ok: true });
  });
});

/**
 * `ClientMsg` (src-tauri/src/sarvam/codec.rs) is the whole of what the app
 * sends: serde writes the `event` tag first, and `audio_input` carries base64
 * of raw 16 kHz PCM16 with no WAV header.
 */
describe("shapeClientFrame", () => {
  const frame = (payload: Uint8Array) => {
    let binary = "";
    for (const b of payload) binary += String.fromCharCode(b);
    return `{"event":"audio_input","audio":"${btoa(binary)}"}`;
  };

  it("passes the app's own frames through as they are, measuring the audio", () => {
    // The literal the app's codec test pins: "QUJD" is the three bytes "ABC".
    expect(shapeClientFrame('{"event":"audio_input","audio":"QUJD"}')).toEqual({ text: '{"event":"audio_input","audio":"QUJD"}', audioBytes: 3 });
    for (const [bytes, padding] of [[32_000, "one"], [16_000, "two"], [3_200, "no"]] as const) {
      const f = frame(new Uint8Array(bytes));
      expect(shapeClientFrame(f), `${padding} '=' of padding`).toEqual({ text: f, audioBytes: bytes });
    }
    // An empty payload is still JSON, and forwarded as the slow path would.
    expect(shapeClientFrame('{"event":"audio_input","audio":""}')).toEqual({ text: '{"event":"audio_input","audio":""}', audioBytes: 0 });
    for (const control of ["speech_start", "speech_end", "end", "ping"]) {
      const f = `{"event":"${control}"}`;
      expect(shapeClientFrame(f)).toEqual({ text: f, audioBytes: 0 });
    }
  });

  it("re-writes an allowed frame in the app's form, dropping every other key", () => {
    expect(shapeClientFrame('{ "audio" : "QUJD", "event" : "audio_input", "prompt": "x" }')).toEqual({
      text: '{"event":"audio_input","audio":"QUJD"}',
      audioBytes: 3,
    });
    // An escaped event name is still the event it names, and is measured.
    expect(shapeClientFrame(String.raw`{"event":"audio\u005finput","audio":"QUJDRA=="}`)).toEqual({
      text: '{"event":"audio_input","audio":"QUJDRA=="}',
      audioBytes: 4,
    });
    expect(shapeClientFrame('{"event":"end","reason":"x"}')).toEqual({ text: '{"event":"end"}', audioBytes: 0 });
  });

  it("refuses anything the app does not send", () => {
    for (const other of [
      // Declared but never sent by the app: it never produces a session.end (codec.rs, `Flush`).
      '{"event":"flush"}',
      '{"event":"config.update","prompt":"x"}',
      '{"event":"transcript.final","text":"x"}',
      '{"event":"audio_input"}',
      '{"event":"audio_input","audio":5}',
      '{"event":"audio_input","audio":"not base64!"}',
      '{"event":"audio_input","audio":"QUJD\\"}',
      // The app's prefix and closing `"}` sharing one quote: not JSON.
      '{"event":"audio_input","audio":"}',
      '{"audio":"QUJD"}',
      '["audio_input"]',
      "null",
      "7",
      "not json",
      "",
    ]) {
      expect(shapeClientFrame(other), other).toBeNull();
    }
    expect(shapeClientFrame(new ArrayBuffer(640))).toBeNull();
    expect(shapeClientFrame(new Uint8Array(640))).toBeNull();
  });
});
