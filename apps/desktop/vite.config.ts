import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Monaco's package exports map only publishes types for the package root, so
// deep imports such as `monaco-editor/esm/vs/editor/editor.api` cannot be
// resolved through it. Aliasing the `esm/` prefix lets the desktop import the
// editor core and individual language tokenizers directly, which keeps Monaco
// served from the local bundle (the app is offline) and avoids pulling in the
// worker-backed language services a read-only viewer does not need.
const monacoEsm = fileURLToPath(new URL("./node_modules/monaco-editor/esm/", import.meta.url));

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  resolve: {
    alias: [{ find: /^monaco-editor\/esm\//, replacement: monacoEsm }],
  },
  server: {
    port: 1420,
    strictPort: true,
    host: "127.0.0.1",
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "es2022",
    chunkSizeWarningLimit: 4096,
  },
  worker: {
    format: "es",
  },
  optimizeDeps: {
    exclude: ["monaco-editor"],
  },
});
