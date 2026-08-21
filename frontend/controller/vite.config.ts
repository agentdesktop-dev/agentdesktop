import path from "node:path";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@brand": path.resolve(import.meta.dirname, "../../images"),
    },
    dedupe: ["react", "react-dom", "lucide-react"],
  },
  clearScreen: false,
  server: {
    strictPort: true,
    proxy: {
      "/api": "http://127.0.0.1:8080",
    },
  },
});
