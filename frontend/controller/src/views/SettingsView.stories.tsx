import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, userEvent } from "storybook/test";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import { controllerSettings } from "../stories/fixtures";
import { SettingsView } from "./SettingsView";

const meta = {
  title: "Controller/Settings",
  component: SettingsView,
  decorators: [
    (Story) => (
      <ControllerStoryFrame path="/settings">
        <Story />
      </ControllerStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: { data: controllerSettings },
} satisfies Meta<typeof SettingsView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const AllCapabilities: Story = {};

export const OptionalCapabilitiesDisabled: Story = {
  args: {
    data: {
      ...controllerSettings,
      gateway_jwt_enabled: false,
      oidc_enabled: false,
      tls_enabled: false,
    },
  },
};

export const ChangesColorMode: Story = {
  play: async ({ canvas, canvasElement }) => {
    const select = canvas.getByRole("combobox", { name: "Color mode" });
    await expect(select).toHaveValue("system");
    await userEvent.selectOptions(select, "dark");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "dark",
    );
    await expect(canvas.getByAltText("Agentdesktop")).toHaveAttribute(
      "src",
      expect.stringContaining("logo-light"),
    );
    await userEvent.selectOptions(select, "light");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "light",
    );
  },
};

export const LoadingCapabilities: Story = {
  args: { data: null, loading: true },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toBeEnabled();
  },
};

export const UnavailableCapabilities: Story = {
  args: { data: null, error: "The controller is unavailable." },
  play: async ({ canvas, canvasElement }) => {
    await userEvent.selectOptions(
      canvas.getByRole("combobox", { name: "Color mode" }),
      "dark",
    );
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "dark",
    );
  },
};

export const Dark: Story = {
  globals: { colorMode: "dark" },
};

export const DarkReflow: Story = {
  globals: {
    colorMode: "dark",
    viewport: { value: "mobile", isRotated: false },
  },
};
