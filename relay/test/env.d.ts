/// <reference types="@cloudflare/vitest-pool-workers/types" />

// The pool's `env` is `Cloudflare.Env`. Pointing it at the Worker's own `Env`
// through `Pick` keeps the two in step without the direct `extends` that
// TypeScript drops here (the object's `DurableObject<Env>` makes that a
// circular base reference).
type RelayEnv = import("../src/index").Env;

declare namespace Cloudflare {
  interface Env extends Pick<RelayEnv, keyof RelayEnv> {}
}
