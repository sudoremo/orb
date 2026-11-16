import { readFileSync } from "node:fs";
import { defineConfig } from "vite";

// Which Font Awesome icons the brands font and the free regular font have,
// read from the package's own metadata at build time so the five megabytes of
// it never reach the bundle.
type Family = { aliases?: { names?: string[] }; svgs: Record<string, Record<string, unknown>> };
const families: Record<string, Family> = JSON.parse(
  readFileSync(
    new URL("node_modules/@fortawesome/fontawesome-free/metadata/icon-families.json", import.meta.url),
    "utf8",
  ),
);
const iconsIn = (style: string): string[] =>
  Object.entries(families)
    .filter(([, family]) => style in (family.svgs.classic ?? {}))
    .flatMap(([name, family]) => [name, ...(family.aliases?.names ?? [])])
    .map((name) => `fa-${name}`);

// Tauri drives this dev server; fail loudly rather than hopping ports so the
// Rust side's `devUrl` always matches.
export default defineConfig({
  clearScreen: false,
  define: {
    __FA_BRANDS__: JSON.stringify(iconsIn("brands")),
    __FA_REGULAR__: JSON.stringify(iconsIn("regular")),
  },
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    rollupOptions: {
      // A draft opens in a window of its own, which needs a page of its own.
      // Sharing the main page would mean loading the message list, the folder
      // tree and the sync machinery into every compose window.
      input: {
        main: "index.html",
        compose: "compose.html",
        console: "console.html",
      },
    },
  },
});
