import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, userEvent } from "storybook/test";

import { DesktopStoryFrame } from "../stories/DesktopStoryFrame";
import { emptyDiscovery, populatedDiscovery } from "../stories/fixtures";
import { ToolsView } from "./ToolsView";

const meta = {
  title: "Desktop/Tools",
  component: ToolsView,
  decorators: [
    (Story) => (
      <DesktopStoryFrame pageTitle="Tools" view="tools">
        <Story />
      </DesktopStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    discovery: populatedDiscovery,
    unavailable: false,
  },
} satisfies Meta<typeof ToolsView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Populated: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByText("VS Code"));
    await expect(
      canvas.getAllByRole("heading", { name: "Skills" })[0],
    ).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Next Skills page" }),
    );
    await expect(canvas.getByText("Workflow 6")).toBeVisible();
  },
};

export const Empty: Story = {
  args: { discovery: emptyDiscovery },
};

export const Unavailable: Story = {
  args: { discovery: null, unavailable: true },
};

export const ReflowAt320: Story = {
  args: { discovery: emptyDiscovery },
  globals: {
    viewport: { value: "reflow", isRotated: false },
  },
  play: async ({ canvasElement }) => {
    const documentElement = canvasElement.ownerDocument.documentElement;
    const viewportWidth = canvasElement.ownerDocument.defaultView?.innerWidth;
    await expect(documentElement.scrollWidth).toBeLessThanOrEqual(
      viewportWidth ?? 0,
    );
  },
};
