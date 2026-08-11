import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    strictPort: true,
    proxy: {
      "/api": "http://127.0.0.1:8080",
    },
  },
});
