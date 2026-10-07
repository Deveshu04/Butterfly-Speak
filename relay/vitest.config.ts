import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    // Generous: a real failure still fails, it only takes longer to say so,
    // and a loaded machine must not turn a slow pass into a timeout.
    testTimeout: 60_000,
  },
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.toml" },
      miniflare: {
        // Stand-ins for the three Wrangler secrets. No real key is ever needed
        // to run the suite: every upstream call is intercepted in-test.
        bindings: {
          SARVAM_API_KEY: "test-sarvam-key",
          SUPABASE_SERVICE_ROLE_KEY: "test-service-role-key",
          CARRY_KEY: "test-carry-key",
        },
      },
    }),
  ],
});
