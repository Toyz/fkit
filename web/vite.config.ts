import { defineConfig } from "vite";

export default defineConfig({
  esbuild: {
    jsx: "automatic",
    jsxImportSource: "@toyz/loom",
    target: "es2022",
    // Loom's decorators key off class and method names, so minification must
    // not rename them.
    keepNames: true,
  },
  build: { target: "es2022", outDir: "dist", emptyOutDir: true },
  server: {
    // `npm run dev` talks to a locally running fkit-hub.
    proxy: {
      "/api": { target: "http://127.0.0.1:7500", changeOrigin: true },
      // The Go module proxy is deliberately not under /api — the toolchain
      // fetches the base URL it was handed in a meta tag, verbatim.
      "/gomod": { target: "http://127.0.0.1:7500", changeOrigin: true },
    },
  },
});
