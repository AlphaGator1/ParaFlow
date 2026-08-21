import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // Tauri drives the dev server on a fixed port and needs a hard failure
  // rather than a silent port bump, or the webview loads nothing.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**", "**/models/**", "**/vendor/**"] },
  },
});
