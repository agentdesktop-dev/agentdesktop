import type { Preview } from "@storybook/react-vite";

import "../src/styles.css";
import "@agentdesktop/ui/styles.css";

const desktopViewports = {
  native: {
    name: "Native window (1080 × 680)",
    styles: { width: "1080px", height: "680px" },
    type: "desktop" as const,
  },
  minimum: {
    name: "Minimum window (720 × 500)",
    styles: { width: "720px", height: "500px" },
    type: "desktop" as const,
  },
  reflow: {
    name: "200% reflow (320 × 500)",
    styles: { width: "320px", height: "500px" },
    type: "mobile" as const,
  },
};

const preview = {
  parameters: {
    layout: "fullscreen",
    a11y: { test: "error" },
    viewport: { options: desktopViewports },
  },
  initialGlobals: {
    viewport: { value: "native", isRotated: false },
  },
} satisfies Preview;

export default preview;
