import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent } from "storybook/test";

import { DesktopStoryFrame } from "../stories/DesktopStoryFrame";
import {
  managedDaemonInfo,
  pendingDevice,
  rejectedDevice,
  unconfiguredDevice,
} from "../stories/fixtures";
import { EnrollmentView } from "./EnrollmentView";

const meta = {
  title: "Desktop/Enrollment",
  component: EnrollmentView,
  decorators: [
    (Story) => (
      <DesktopStoryFrame fullWidth pageTitle="Enrollment">
        <Story />
      </DesktopStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    busy: false,
    daemon: managedDaemonInfo,
    enrollment: unconfiguredDevice,
    onCopy: fn(),
    onEnroll: fn(),
  },
} satisfies Meta<typeof EnrollmentView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ReadyToEnroll: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: "Enroll device" }),
    );
    await expect(args.onEnroll).toHaveBeenCalledOnce();
  },
};

export const OpeningSignIn: Story = {
  args: { busy: true },
};

export const PendingApproval: Story = {
  args: { enrollment: pendingDevice },
};

export const Rejected: Story = {
  args: { enrollment: rejectedDevice },
};

export const ConnectionDetailsBeforeEnrollment: Story = {
  args: { enrollment: rejectedDevice },
  play: async ({ args, canvas }) => {
    await userEvent.click(canvas.getByText("Advanced"));
    await expect(
      canvas.getByRole("region", { name: "Daemon information" }),
    ).toBeVisible();
    await expect(canvas.getByText(managedDaemonInfo.configPath)).toBeVisible();
    await expect(
      canvas.getByText("https://controller.example.internal:8443/"),
    ).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Copy diagnostics" }),
    );
    await expect(args.onCopy).toHaveBeenCalledOnce();
  },
};
