import {
  applyColorMode,
  BrowserThemeProvider,
  COLOR_MODE_STORAGE_KEY,
  readColorMode,
} from "@agentdesktop/ui";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./styles.css";
import "@agentdesktop/ui/styles.css";

applyColorMode(readColorMode(COLOR_MODE_STORAGE_KEY));

const root = document.getElementById("root");

if (root) {
  createRoot(root).render(
    <StrictMode>
      <BrowserThemeProvider>
        <App />
      </BrowserThemeProvider>
    </StrictMode>,
  );
}
