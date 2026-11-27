import { cpSync } from "node:fs";
import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// Emoji pictures ship inside the app (it works offline): Twemoji SVGs, served as /emoji/*.svg.
cpSync("node_modules/@twemoji/svg", "public/emoji", { recursive: true, filter: (f) => !f.endsWith(".json") });

export default defineConfig({
  plugins: [solid()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
});
