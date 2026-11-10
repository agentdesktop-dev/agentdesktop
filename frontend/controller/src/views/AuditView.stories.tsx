import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";

import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import { auditEvents } from "../stories/fixtures";
import { AuditView } from "./AuditView";

const meta = {
  title: "Controller/Audit",
  component: AuditView,
  decorators: [
    (Story) => (
      <ControllerStoryFrame path="/audit">
        <Story />
      </ControllerStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: { events: auditEvents, onLoadMore: fn() },
} satisfies Meta<typeof AuditView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Populated: Story = {
  play: async ({ canvas }) => {
    await expect(canvas.getByText("Permission denied")).toBeVisible();
    await expect(canvas.getByText("Denied")).toBeVisible();
    await expect(canvas.getByText("sam.okafor@example.com")).toBeVisible();
    await expect(canvas.getByText("Signed in")).toBeVisible();
    await expect(canvas.getByText("devices.enroll")).toBeVisible();
  },
};

export const ShowsTheFullRecord: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: "Show details for event 3" }),
    );
    await expect(canvas.getByText(/"status": 204/)).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Hide details for event 3" }),
    );
    await expect(canvas.queryByText(/"status": 204/)).not.toBeInTheDocument();
  },
};

export const LoadsOlderEvents: Story = {
  args: { hasMore: true },
  play: async ({ args, canvas }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: "Load older events" }),
    );
    await expect(args.onLoadMore).toHaveBeenCalledOnce();
  },
};

export const LoadingOlderEvents: Story = {
  args: { hasMore: true, loadingMore: true },
};

export const Empty: Story = {
  args: { events: [] },
  play: async ({ canvas }) => {
    await expect(canvas.getByText("No audit events yet")).toBeVisible();
  },
};

export const Loading: Story = {
  args: { events: [], loading: true },
};

export const LoadError: Story = {
  args: {
    events: [],
    error: "You do not have permission to view the audit log.",
  },
};

export const MobileReflow: Story = {
  globals: { viewport: { value: "mobile", isRotated: false } },
  play: async ({ canvasElement }) => {
    const documentElement = canvasElement.ownerDocument.documentElement;
    const viewportWidth = canvasElement.ownerDocument.defaultView?.innerWidth;
    await expect(documentElement.scrollWidth).toBeLessThanOrEqual(
      viewportWidth ?? 0,
    );
  },
};
