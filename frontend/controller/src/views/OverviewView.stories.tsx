import type { Meta, StoryObj } from "@storybook/react-vite";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import { overview } from "../stories/fixtures";
import { OverviewView } from "./OverviewView";

const meta = {
  title: "Controller/Overview",
  component: OverviewView,
  decorators: [
    (Story) => (
      <ControllerStoryFrame path="/">
        <Story />
      </ControllerStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: { data: overview },
} satisfies Meta<typeof OverviewView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const PopulatedFleet: Story = {};

export const EmptyFleet: Story = {
  args: {
    data: {
      ...overview,
      total_devices: 0,
      online_devices: 0,
      offline_devices: 0,
      config_failures: 0,
      recent_devices: [],
    },
  },
};

export const NoActiveConfiguration: Story = {
  args: { data: { ...overview, active_revision: null } },
};
