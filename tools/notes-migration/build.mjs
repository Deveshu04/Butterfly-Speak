// Bundles src/lib/notes/scratchpadMigration.ts for the harness, with the
// `$lib/api` import aliased to ./api-stub.js.
//
// esbuild is resolved out of the repo's pnpm store rather than imported,
// because pnpm's non-hoisted layout means `import "esbuild"` does not resolve
// from this directory.

import { execFileSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");

function findEsbuild() {
  const direct = join(repo, "node_modules", "esbuild", "bin", "esbuild");
  if (existsSync(direct)) return direct;
  const store = join(repo, "node_modules", ".pnpm");
  if (!existsSync(store)) throw new Error("no node_modules — run `pnpm install` first");
  const pkg = readdirSync(store).find((d) => d.startsWith("esbuild@"));
  if (!pkg) throw new Error("esbuild not found in the pnpm store");
  return join(store, pkg, "node_modules", "esbuild", "bin", "esbuild");
}

execFileSync(
  process.execPath,
  [
    findEsbuild(),
    join(repo, "src", "lib", "notes", "scratchpadMigration.ts"),
    "--bundle",
    "--format=esm",
    "--platform=node",
    `--outfile=${join(here, "bundle.mjs")}`,
    "--external:./api-stub.js",
    "--alias:$lib/api=./api-stub.js",
  ],
  { stdio: "inherit" },
);
