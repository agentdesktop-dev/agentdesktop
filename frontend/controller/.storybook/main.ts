import type { StorybookConfig } from "@storybook/react-vite";

const config = {
  stories: ["../src/**/*.stories.@(ts|tsx)"],
  addons: ["@storybook/addon-a11y", "@storybook/addon-vitest"],
  framework: {
    name: "@storybook/react-vite",
    options: {},
  },
  viteFinal: async (viteConfig) => {
    viteConfig.build = { ...viteConfig.build, target: "es2022" };
    return viteConfig;
  },
} satisfies StorybookConfig;

export default config;
