import "@fontsource-variable/manrope";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { Desktop } from "./Desktop";
import "./styles.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Desktop />
  </StrictMode>
);