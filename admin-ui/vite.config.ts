import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  base: "./",
  plugins: [react()],
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 1430,
    strictPort: true,
    proxy: {
      "/v1": process.env.ADMIN_UI_SERVER_URL || "http://127.0.0.1:8091"
    }
  },
  build: {
    target: "es2022",
    outDir: "../control-plane/internal/adminui/static",
    emptyOutDir: true
  }
});
