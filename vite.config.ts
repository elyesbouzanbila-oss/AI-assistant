import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Extra hostnames allowed to reach the dev server (needed when previewing
// from a remote VM). Unset on a normal desktop, which keeps the default
// localhost-only behaviour.
const extraHosts = (process.env.VITE_ALLOWED_HOSTS ?? "")
  .split(",")
  .map((h: string) => h.trim())
  .filter(Boolean);

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    host: "0.0.0.0",
    port: 5173,
    strictPort: true,
    allowedHosts: extraHosts.length > 0 ? extraHosts : undefined,
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "chrome110",
  },
});
