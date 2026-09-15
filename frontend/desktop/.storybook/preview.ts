import { parseColorMode, ThemeProvider } from "@agentdesktop/ui";
import type { Preview } from "@storybook/react-vite";
import { createElement } from "react";

import "../src/styles.css";
import "@agentdesktop/ui/styles.css";

const desktopViewports = {
  native: {
    name: "Native window (940 × 680)",
    styles: { width: "940px", height: "680px" },
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
  globalTypes: {
    colorMode: {
      description: "Interface appearance",
      toolbar: {
        icon: "circlehollow",
        dynamicTitle: true,
        items: [
          { value: "system", title: "System" },
          { value: "light", title: "Light" },
          { value: "dark", title: "Dark" },
        ],
      },
    },
  },
  decorators: [
    (Story, context) =>
      context.parameters.themeProvider === false
        ? createElement(Story)
        : createElement(
            ThemeProvider,
            {
              key: `${context.id}-${context.globals.colorMode}`,
              defaultMode: parseColorMode(context.globals.colorMode),
            },
            createElement(Story),
          ),
  ],
  parameters: {
    layout: "fullscreen",
    a11y: { test: "error" },
    viewport: { options: desktopViewports },
  },
  initialGlobals: {
    colorMode: "system",
    viewport: { value: "native", isRotated: false },
  },
} satisfies Preview;

export default preview;
