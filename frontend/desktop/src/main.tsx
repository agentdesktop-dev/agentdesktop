import { applyColorMode } from "@agentdesktop/ui";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { Desktop } from "./Desktop";
import "./styles.css";
import "@agentdesktop/ui/styles.css";

// Native startup restores the saved window appearance before showing it.
applyColorMode("system");

const root = document.getElementById("root");
if (!root) throw new Error("Missing application root");

createRoot(root).render(
  <StrictMode>
    <Desktop />
  </StrictMode>,
);
