import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// D-001: Vite is the dev server / bundler only. Nothing here ships a JS runtime —
// the build output is static assets the Tauri Rust process serves into the OS
// webview.
export default defineConfig({
  plugins: [react()],
  // Tauri expects a fixed port and fails rather than silently picking another.
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // D-019: WebKitGTK is the compatibility floor, so do not emit syntax the
    // oldest supported webview cannot parse.
    target: "es2022",
    sourcemap: true,
  },
});
