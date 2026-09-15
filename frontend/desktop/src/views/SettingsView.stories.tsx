import { SYSTEM_THEME_QUERY } from "@agentdesktop/ui";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";

import { DesktopStoryFrame } from "../stories/DesktopStoryFrame";
import { bootstrap } from "../stories/fixtures";
import { SettingsView } from "./SettingsView";

const meta = {
  title: "Desktop/Settings",
  component: SettingsView,
  decorators: [
    (Story) => (
      <DesktopStoryFrame pageTitle="Settings" view="settings">
        <Story />
      </DesktopStoryFrame>
    ),
  ],
  tags: ["test"],
  args: {
    settings: bootstrap.settings,
    disabled: false,
    onStartupChange: fn(),
  },
} satisfies Meta<typeof SettingsView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const System: Story = {
  play: async ({ canvas, canvasElement }) => {
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toHaveValue("system");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      window.matchMedia(SYSTEM_THEME_QUERY).matches ? "dark" : "light",
    );
  },
};

export const ChangesColorMode: Story = {
  play: async ({ canvas, canvasElement }) => {
    const select = canvas.getByRole("combobox", { name: "Color mode" });
    const root = canvasElement.ownerDocument.documentElement;
    for (const mode of ["dark", "light", "system"]) {
      await userEvent.selectOptions(select, mode);
      const theme =
        mode === "system"
          ? window.matchMedia(SYSTEM_THEME_QUERY).matches
            ? "dark"
            : "light"
          : mode;
      await expect(select).toHaveValue(mode);
      await expect(root).toHaveAttribute("data-theme", theme);
      await expect(root).toHaveStyle({ colorScheme: theme });
    }
  },
};

export const ChangesStartupPreference: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(
      canvas.getByRole("checkbox", { name: "Open window at startup" }),
    );
    await expect(args.onStartupChange).toHaveBeenCalledWith(false);
  },
};

export const Saving: Story = {
  args: { disabled: true },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toBeDisabled();
    await expect(
      canvas.getByRole("checkbox", { name: "Open window at startup" }),
    ).toBeDisabled();
  },
};

export const Dark: Story = {
  globals: { colorMode: "dark" },
};

export const DarkReflow: Story = {
  globals: {
    colorMode: "dark",
    viewport: { value: "reflow", isRotated: false },
  },
};
