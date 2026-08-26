import { defineConfig } from "vite";
import { sveltekit } from "@sveltejs/kit/vite";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;
// @ts-expect-error process is a nodejs global
const serverUrl = process.env.PUPPETTERM_SERVER_URL || "http://127.0.0.1:8080";

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [sveltekit()],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
    // Browser mode (no Tauri) proxies API + WebSocket to the headless
    // puppetterm-server. Override the target with PUPPETTERM_SERVER_URL.
    proxy: {
      "/api": {
        target: serverUrl,
        changeOrigin: true,
      },
      "/ws": {
        target: serverUrl,
        ws: true,
        changeOrigin: true,
      },
    },
  },
}));
