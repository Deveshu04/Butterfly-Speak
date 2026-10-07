import { createRemoteJWKSet, errors, jwtVerify } from "jose";
import type { JWTPayload, JWTVerifyGetKey } from "jose";
import { carryKey } from "./carry";
import type { Carry } from "./carry";
import type { UserSession } from "./user_session";

export { Carry } from "./carry";
export { UserSession } from "./user_session";

export interface Env {
  USER_SESSION: DurableObjectNamespace<UserSession>;
  CARRY: DurableObjectNamespace<Carry>;
  SUPABASE_URL: string;
  WEEKLY_WORD_LIMIT: string;
  WEEKLY_CHAT_LIMIT: string;
  WEEKLY_AUDIO_SECONDS: string;
  SESSION_MAX_SECONDS: string;
  SESSION_IDLE_SECONDS: string;
  SARVAM_REALTIME_URL: string;
  SARVAM_CHAT_URL: string;
  SARVAM_API_KEY: string;
  SUPABASE_SERVICE_ROLE_KEY: string;
  /** The secret the carry key is made with. Without it nothing is carried. */
  CARRY_KEY?: string;
}

/**
 * The remote key set is cached for the isolate's life (jose re-fetches on its
 * own every `cacheMaxAge`), keyed by URL so a staging deployment pointed at a
 * different project cannot reuse the wrong keys.
 */
let jwksUrl: string | null = null;
let jwks: ReturnType<typeof createRemoteJWKSet> | null = null;

function keySet(env: Env): ReturnType<typeof createRemoteJWKSet> {
  const url = `${env.SUPABASE_URL}/auth/v1/.well-known/jwks.json`;
  if (!jwks || jwksUrl !== url) {
    jwksUrl = url;
    jwks = createRemoteJWKSet(new URL(url), { cacheMaxAge: 600_000 });
  }
  return jwks;
}

/**
 * Raised in place of whatever went wrong while *reading the key set* -- a JWKS
 * fetch that never landed, or one that timed out. That says nothing about the
 * token, so it must not end the user's session.
 */
class JwksUnavailable extends Error {}

/**
 * Distinguish "we could not read the key set" from "the key set does not hold
 * this token's key". Only the first is our outage. A `kid` that is not in the
 * cached set -- forged, rotated out, or published since the last fetch --
 * raises `JWKSNoMatchingKey`, and that is a `401`. jose fetches the set again
 * first, but not within 30 s of its last fetch (its `cooldownDuration`), so a
 * key published in that time is refused until then. The failures `fetchJwks`
 * raises for a set it could not read are `JWKSTimeout` and two generic ones (a
 * non-200 response, a body that is not JSON); a network throw is not a jose
 * error at all.
 */
function isKeySetUnreadable(err: unknown): boolean {
  if (!(err instanceof errors.JOSEError)) return true;
  return err instanceof errors.JWKSTimeout || err.code === "ERR_JOSE_GENERIC";
}

type Auth = { ok: true; sub: string; email: string | null } | { ok: false; status: 401 | 503 };

/**
 * Cloud quota belongs to Google identities only. With the Email provider
 * enabled, anyone holding the public anon key could sign up email identities
 * at will, each with its own weekly allowance; the project keeps it off, and
 * this check is defence in depth behind that. The provider is read from
 * `app_metadata`, which only Supabase Auth writes; `user_metadata` is the
 * user's own to edit and is never consulted. An account
 * first created by email and later linked to Google keeps `provider: "email"`
 * and is refused.
 */
function isGoogleIdentity(payload: JWTPayload): boolean {
  const app = payload.app_metadata;
  const provider = typeof app === "object" && app !== null ? (app as Record<string, unknown>).provider : undefined;
  return provider === "google" && payload.is_anonymous !== true;
}

async function authenticate(req: Request, env: Env): Promise<Auth> {
  // The header only. A bearer in the URL would land in access logs and
  // histories, and neither the app nor the harness ever sends one there.
  const h = req.headers.get("authorization") ?? "";
  const token = h.startsWith("Bearer ") ? h.slice(7) : "";
  if (!token) return { ok: false, status: 401 };
  const keys = keySet(env);
  const resolve: JWTVerifyGetKey = async (header, jws) => {
    try {
      return await keys(header, jws);
    } catch (err) {
      if (!isKeySetUnreadable(err)) throw err; // the token's problem: let it be a 401
      throw new JwksUnavailable(err instanceof Error ? err.name : "jwks");
    }
  };
  try {
    const { payload } = await jwtVerify(token, resolve, {
      issuer: `${env.SUPABASE_URL}/auth/v1`,
      audience: "authenticated",
      // Defence in depth: the live key set is a single ES256 P-256 key, and no
      // token may choose a weaker algorithm (or `none`) for itself.
      algorithms: ["ES256", "RS256"],
    });
    return typeof payload.sub === "string" && payload.role === "authenticated" && isGoogleIdentity(payload)
      ? { ok: true, sub: payload.sub, email: typeof payload.email === "string" ? payload.email : null }
      : { ok: false, status: 401 };
  } catch (err) {
    if (err instanceof JwksUnavailable) {
      console.warn(`jwks unavailable: ${err.message}`);
      return { ok: false, status: 503 };
    }
    return { ok: false, status: 401 };
  }
}

/**
 * The name of a user's Durable Object. There is deliberately no location hint:
 * an object is created next to the Cloudflare location that first asks for it,
 * which is where that user enters Cloudflare, so the hop from the Worker to the
 * object stays short wherever the user's network lands them. (A fixed "apac"
 * hint would send a user who enters at Marseille to Asia and back on every
 * request.) The `user:` prefix namespaces the per-user objects.
 */
export function objectName(sub: string): string {
  return `user:${sub}`;
}

let carryDisabledSaid = false;

/**
 * The carry key for the verified address, or null when carrying is off or
 * the token has no address. The key is all the user's object ever learns of
 * the address.
 */
async function carryKeyFor(env: Env, email: string | null): Promise<string | null> {
  if (!env.CARRY_KEY) {
    if (!carryDisabledSaid) {
      carryDisabledSaid = true;
      console.warn("carry disabled");
    }
    return null;
  }
  if (!email || email.trim() === "") return null;
  return await carryKey(env.CARRY_KEY, email);
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const url = new URL(req.url);
    if (!url.pathname.startsWith("/v1/")) return new Response("not found", { status: 404 });
    const auth = await authenticate(req, env);
    if (!auth.ok) {
      // A key set we could not reach is our outage, not a dead session: say so,
      // so the app retries instead of signing the user out.
      return auth.status === 503
        ? new Response("key set unavailable", { status: 503, headers: { "retry-after": "5" } })
        : new Response("unauthorized", { status: 401 });
    }
    const sub = auth.sub;
    const stub = env.USER_SESSION.get(env.USER_SESSION.idFromName(objectName(sub)));
    // The object never sees the token: the header is replaced by the verified
    // subject, and by the carry key of the verified address.
    const headers = new Headers(req.headers);
    headers.set("x-user-id", sub);
    headers.delete("authorization");
    headers.delete("x-carry-key");
    const carry = await carryKeyFor(env, auth.email);
    if (carry) headers.set("x-carry-key", carry);
    const fwd = new Request(url.toString(), {
      method: req.method,
      headers,
      body: req.method === "GET" || req.method === "HEAD" ? null : req.body,
    });
    return stub.fetch(fwd);
  },
} satisfies ExportedHandler<Env>;
