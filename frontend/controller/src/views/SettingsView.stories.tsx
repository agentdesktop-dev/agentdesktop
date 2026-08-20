import type { Meta, StoryObj } from "@storybook/react-vite";

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
