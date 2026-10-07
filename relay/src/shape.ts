const LANG = /^[a-z]{2}-[A-Z]{2}$|^auto$|^unknown$/;
const STREAM_TYPES = new Set(["balanced", "fast"]);
const MODES = new Set(["transcribe", "translate", "verbatim", "translit", "codemix"]);
const ENDPOINTING = new Set(["manual", "vad"]);
const PROMPT_MAX = 2000;

/**
 * The realtime query the relay sends upstream: allow-listed values, pinned
 * model/encoding/rate, in the order the app's own `ws_url` writes them.
 *
 * Each value goes through `encodeURIComponent`, so a space is `%20` as it is
 * in the URL the app sends Sarvam directly. `URLSearchParams` would write `+`,
 * which is only a space to a form decoder; a multi-word prompt could reach
 * Sarvam as "Priya+Sharma".
 */
export function shapeRealtimeQuery(url: URL): string | null {
  const p = url.searchParams;
  const language = p.get("language_code") ?? "";
  const streamType = p.get("stream_type") ?? "";
  const mode = p.get("mode") ?? "";
  const endpointing = p.get("endpointing") ?? "";
  if (!LANG.test(language) || !STREAM_TYPES.has(streamType) || !MODES.has(mode) || !ENDPOINTING.has(endpointing)) return null;
  const out: [string, string][] = [
    ["model", "saaras:v3-realtime"],
    ["language_code", language],
    ["stream_type", streamType],
    ["mode", mode],
    ["endpointing", endpointing],
    ["encoding", "linear16"],
    ["sample_rate", "16000"],
  ];
  const prompt = p.get("prompt");
  if (prompt) {
    if (prompt.length > PROMPT_MAX) return null;
    out.push(["prompt", prompt]);
  }
  return out.map(([key, value]) => `${key}=${encodeURIComponent(value)}`).join("&");
}

/**
 * The `event`s the app's `ClientMsg` (src-tauri/src/sarvam/codec.rs) sends:
 * `audio_input`, `speech_start`, `speech_end` and `end`, plus `ping`, the
 * keepalive it declares for longer sessions. Not `flush`: the app does not
 * send it, since it never produces a `session.end`.
 */
const CLIENT_EVENTS = new Set(["audio_input", "speech_start", "speech_end", "end", "ping"]);
/** serde's form of `ClientMsg::AudioInput { audio }`: the tag first, then `audio`. */
const APP_AUDIO_PREFIX = '{"event":"audio_input","audio":"';
const APP_AUDIO_SUFFIX = '"}';
const BASE64 = /^[A-Za-z0-9+/]*={0,2}$/;

/** The bytes a base64 string decodes to. */
function base64Bytes(b64: string): number {
  const padding = b64.endsWith("==") ? 2 : b64.endsWith("=") ? 1 : 0;
  return Math.floor(((b64.length - padding) * 3) / 4);
}

export interface ClientFrame {
  /** What goes to Sarvam: always the app's own serialization of the frame. */
  text: string;
  /** Audio bytes it carries: raw 16 kHz PCM16, no WAV header (codec.rs). */
  audioBytes: number;
}

/**
 * The frame to send Sarvam for one frame from the client, or `null` to drop
 * it. Only the events the app sends pass, each rebuilt from its event (and,
 * for audio, its base64 payload) in the exact form serde gives it, so no other
 * key and no other text reaches Sarvam. The app's own audio frame is checked
 * without parsing -- ten a second per session -- and is already in that form;
 * anything else is parsed. Binary frames are dropped: the app sends none, and
 * Sarvam's realtime API takes audio only as base64 text.
 */
export function shapeClientFrame(data: unknown): ClientFrame | null {
  if (typeof data !== "string") return null;
  // Long enough that the prefix's closing quote and the suffix's opening one
  // are two quotes: `{"event":"audio_input","audio":"}` has both ends and is
  // not JSON.
  if (
    data.length >= APP_AUDIO_PREFIX.length + APP_AUDIO_SUFFIX.length &&
    data.startsWith(APP_AUDIO_PREFIX) &&
    data.endsWith(APP_AUDIO_SUFFIX)
  ) {
    const audio = data.slice(APP_AUDIO_PREFIX.length, data.length - APP_AUDIO_SUFFIX.length);
    if (BASE64.test(audio)) return { text: data, audioBytes: base64Bytes(audio) };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(data);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return null;
  const { event, audio } = parsed as Record<string, unknown>;
  if (typeof event !== "string" || !CLIENT_EVENTS.has(event)) return null;
  if (event !== "audio_input") return { text: JSON.stringify({ event }), audioBytes: 0 };
  if (typeof audio !== "string" || !BASE64.test(audio)) return null;
  return { text: JSON.stringify({ event, audio }), audioBytes: base64Bytes(audio) };
}

const CHAT_MODELS = new Set(["sarvam-105b", "sarvam-105b-conversations"]);
const CHAT_KEYS = new Set(["model", "messages", "temperature", "max_tokens", "stream", "stream_options", "reasoning_effort"]);
/**
 * 128 KiB of UTF-8. A long note action in Devanagari -- three bytes a
 * character -- can pass 64 KB legitimately.
 */
export const CHAT_BODY_MAX_BYTES = 128 * 1024;
const MAX_TOKENS_CAP = 8192;
const encoder = new TextEncoder();

/**
 * Bytes, not UTF-16 code units: Devanagari is three bytes a character, so
 * `String.length` under-counts an Indic body by a factor of three.
 */
export function byteLength(text: string): number {
  return encoder.encode(text).length;
}

export function shapeChatBody(json: unknown): { ok: true; body: string } | { ok: false; status: number; reason: string } {
  if (typeof json !== "object" || json === null || Array.isArray(json)) return { ok: false, status: 400, reason: "body must be a JSON object" };
  const o = json as Record<string, unknown>;
  for (const k of Object.keys(o)) if (!CHAT_KEYS.has(k)) return { ok: false, status: 400, reason: `unexpected field ${k}` };
  if (typeof o.model !== "string" || !CHAT_MODELS.has(o.model)) return { ok: false, status: 400, reason: "unsupported model" };
  if (!Array.isArray(o.messages) || o.messages.length === 0 || o.messages.length > 8) return { ok: false, status: 400, reason: "messages" };
  if (o.max_tokens !== undefined && (typeof o.max_tokens !== "number" || o.max_tokens > MAX_TOKENS_CAP || o.max_tokens < 1)) return { ok: false, status: 400, reason: "max_tokens" };
  const body = JSON.stringify(o);
  if (byteLength(body) > CHAT_BODY_MAX_BYTES) return { ok: false, status: 413, reason: "body too large" };
  return { ok: true, body };
}
