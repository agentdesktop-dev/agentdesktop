import type { Preview } from "@storybook/react-vite";

import "../src/styles.css";
import "@agentdesktop/ui/styles.css";

const controllerViewports = {
  desktop: {
    name: "Controller desktop (1280 × 800)",
    styles: { width: "1280px", height: "800px" },
    type: "desktop" as const,
  },
  compact: {
    name: "Compact window (760 × 720)",
    styles: { width: "760px", height: "720px" },
    type: "tablet" as const,
  },
  mobile: {
    name: "Mobile reflow (320 × 640)",
    styles: { width: "320px", height: "640px" },
    type: "mobile" as const,
  },
};

const preview = {
  parameters: {
    layout: "fullscreen",
    a11y: { test: "error" },
    viewport: { options: controllerViewports },
  },
  initialGlobals: {
    viewport: { value: "desktop", isRotated: false },
  },
} satisfies Preview;

export default preview;
