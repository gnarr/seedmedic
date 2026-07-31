import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  // Relative, so the bundle works under a reverse-proxy sub-path without being
  // rebuilt: the shell injects a <base> and every asset URL resolves against it.
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Content-hashed, so /assets/* can be cached immutably forever while
    // index.html stays no-cache.
    assetsDir: "assets",
  },
  server: {
    // `npm run dev` talks to a `cargo run` on 9899. `changeOrigin` stays false so
    // the Host header keeps saying localhost:5173, which makes the Origin
    // fallback in `is_cross_site` agree with Sec-Fetch-Site.
    proxy: {
      "/api": { target: "http://127.0.0.1:9899", changeOrigin: false },
      "/health": { target: "http://127.0.0.1:9899", changeOrigin: false },
    },
  },
});
