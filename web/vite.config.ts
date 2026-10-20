// Two builds from one source:
//
//   vite build              → dist/      the full dashboard a Linux bob serves
//   vite build --mode mcu   → dist-mcu/  only the panels a microcontroller node has
//
// The code is written against React's API and built against preact/compat (aliased below):
// the same components, a ~4 KB runtime instead of ~45 KB. The only difference between the
// builds is which section registry `@registry` points at, so code for panels a build
// doesn't list is never bundled.
//
// Dev: `bun run dev` serves the UI with hot reload. Point it at a bob with `?bob=http://host`
// in the page URL, VITE_BOB=http://host, or BOB_PROXY=http://host to proxy /api, /ws and
// /mcp through the dev server (same-origin, so it also works without CORS).
import { loadEnv } from "vite";
import { defineConfig } from "vitest/config";
import { fileURLToPath } from "node:url";

const here = (p: string) => fileURLToPath(new URL(p, import.meta.url));

export default defineConfig(({ mode }) => {
  const mcu = mode === "mcu";
  const env = loadEnv(mode, here("."), "");
  const proxy = env.BOB_PROXY;
  return {
    // Relative asset URLs: the same build works at a bob's root, under a sub-path, or from
    // a desktop app's bundle.
    base: "./",
    resolve: {
      alias: [
        { find: /^react$/, replacement: "preact/compat" },
        { find: /^react-dom$/, replacement: "preact/compat" },
        { find: /^react-dom\/client$/, replacement: "preact/compat/client" },
        { find: /^react\/jsx-runtime$/, replacement: "preact/jsx-runtime" },
        { find: /^react\/jsx-dev-runtime$/, replacement: "preact/jsx-runtime" },
        { find: /^@registry$/, replacement: here(mcu ? "src/registry.mcu.ts" : "src/registry.full.ts") },
      ],
    },
    define: { __MCU__: JSON.stringify(mcu) },
    build: {
      outDir: mcu ? "dist-mcu" : "dist",
      emptyOutDir: true,
      // The page is served by the bob it talks to; old browsers aren't a goal, small output is.
      target: "es2022",
      // Small node flash: keep the asset count low, inline nothing big.
      assetsInlineLimit: 2048,
      reportCompressedSize: false,
      chunkSizeWarningLimit: 300,
    },
    server: proxy
      ? {
          proxy: {
            "/api": { target: proxy, changeOrigin: true },
            "/mcp": { target: proxy, changeOrigin: true },
            "/ws": { target: proxy, changeOrigin: true, ws: true },
          },
        }
      : undefined,
    test: {
      environment: "happy-dom",
      include: ["src/**/*.test.{ts,tsx}"],
      setupFiles: ["src/test/setup.ts"],
    },
  };
});
