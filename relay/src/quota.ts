/** Monday 00:00 UTC of the ISO week containing `d`, as YYYY-MM-DD. */
export function weekStart(d: Date): string {
  const day = d.getUTCDay(); // 0 = Sunday
  const back = (day + 6) % 7; // days since Monday
  const monday = new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate() - back));
  return monday.toISOString().slice(0, 10);
}

/** Whitespace-separated words; script-agnostic on purpose. */
export function countWords(text: string): number {
  const t = text.trim();
  return t === "" ? 0 : t.split(/\s+/).length;
}

/**
 * 16 kHz mono PCM16 -- the `sample_rate` and `encoding` the relay pins on the
 * upstream socket, so it is how Sarvam reads every byte it is sent: 32 bytes
 * to the millisecond.
 */
export const PCM_BYTES_PER_MS = 32;
