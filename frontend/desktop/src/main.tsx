import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { Desktop } from "./Desktop";
import "./styles.css";
import "@agentdesktop/ui/styles.css";

const root = document.getElementById("root");
if (!root) throw new Error("Missing application root");

createRoot(root).render(
  <StrictMode>
    <Desktop />
  </StrictMode>,
);
