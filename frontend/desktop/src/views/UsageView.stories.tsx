import type { Meta, StoryObj } from "@storybook/react-vite";
import { useState } from "react";
import { expect, fn, userEvent, within } from "storybook/test";

import { DesktopStoryFrame } from "../stories/DesktopStoryFrame";
import { llmUsage } from "../stories/fixtures";
import type {
  LlmUsageInteractions,
  LlmUsageRange,
  LlmUsageSummary,
} from "../types";
import { UsageView, type UsageViewProps } from "./UsageView";

const interactionPage: LlmUsageInteractions = {
  interactions: [
    {
      id: "request-1",
      startedAt: "2026-09-07T06:09:23Z",
      completedAt: "2026-09-07T06:09:29Z",
      durationMs: 6038,
      httpStatus: 200,
      failed: false,
      operation: "chat",
      provider: "copilot",
      requestModel: "gpt-5.6-sol",
      responseModel: "gpt-5.6-sol",
      agent: "GitHubCopilotChat",
      inputTokens: 363_813,
      outputTokens: 341,
      totalTokens: 364_154,
      estimatedCostUsd: 0.179343,
    },
    {
      id: "request-2",
      startedAt: "2026-09-07T06:08:30Z",
      completedAt: "2026-09-07T06:08:48Z",
      durationMs: 17_997,
      httpStatus: 200,
      failed: false,
      operation: "chat",
      provider: "copilot",
      requestModel: "gpt-5.6-sol",
      responseModel: "gpt-5.6-sol",
      agent: "GitHubCopilotChat",
      inputTokens: null,
      outputTokens: null,
      totalTokens: null,
      estimatedCostUsd: 0,
    },
  ],
  nextCursor: "older-page",
};

const olderInteractionPage: LlmUsageInteractions = {
  interactions: [
    {
      id: "request-3",
      startedAt: "2026-09-07T06:07:10Z",
      completedAt: "2026-09-07T06:07:11Z",
      durationMs: 745,
      httpStatus: 429,
      failed: true,
      operation: "chat",
      provider: "copilot",
      requestModel: "gpt-5.6-sol",
      responseModel: null,
      agent: "GitHubCopilotChat",
      inputTokens: null,
      outputTokens: null,
      totalTokens: null,
      estimatedCostUsd: null,
    },
  ],
  nextCursor: null,
};

const usageByRange: Record<LlmUsageRange, LlmUsageSummary> = {
  hour: scaleUsage(0.08, "2026-09-03T11:00:00Z"),
  day: llmUsage,
  week: scaleUsage(7, "2026-08-27T12:00:00Z"),
  month: scaleUsage(30, "2026-08-04T12:00:00Z"),
};

function scaleUsage(factor: number, from: string): LlmUsageSummary {
  const breakdown = llmUsage.breakdown.map((item) => ({
    ...item,
    requests: Math.max(1, Math.round(item.requests * factor)),
    totalTokens: Math.round(item.totalTokens * factor),
    estimatedCostUsd: item.estimatedCostUsd * factor,
  }));
  return {
    from,
    to: llmUsage.to,
    requests: breakdown.reduce((total, item) => total + item.requests, 0),
    totalTokens: breakdown.reduce((total, item) => total + item.totalTokens, 0),
    estimatedCostUsd: breakdown.reduce(
      (total, item) => total + item.estimatedCostUsd,
      0,
    ),
    breakdown,
  };
}

function InteractiveUsageView({
  onRangeChange,
  onLoadInteractions,
}: Pick<UsageViewProps, "onRangeChange" | "onLoadInteractions">) {
  const [range, setRange] = useState<LlmUsageRange>("day");
  return (
    <UsageView
      llmUsage={usageByRange[range]}
      range={range}
      loadedRange={range}
      isRangeLoading={false}
      onLoadInteractions={onLoadInteractions}
      onRangeChange={(nextRange) => {
        setRange(nextRange);
        onRangeChange(nextRange);
      }}
    />
  );
}

const meta = {
  title: "Desktop/Usage",
  component: UsageView,
  decorators: [
    (Story) => (
      <DesktopStoryFrame pageTitle="Usage" view="usage">
        <Story />
      </DesktopStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    llmUsage,
    range: "day",
    loadedRange: "day",
    isRangeLoading: false,
    onRangeChange: fn(),
    onLoadInteractions: fn(async (_from, _to, _model, _agent, cursor) =>
      cursor ? olderInteractionPage : interactionPage,
    ),
  },
} satisfies Meta<typeof UsageView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Ready: Story = {
  render: (args) => (
    <InteractiveUsageView
      onRangeChange={args.onRangeChange}
      onLoadInteractions={args.onLoadInteractions}
    />
  ),
  play: async ({ canvas }) => {
    await expect(canvas.getByText("Gateway usage")).toBeVisible();
    await expect(
      canvas.getByText("Past 24 hours", { exact: false }),
    ).toBeVisible();
    await expect(
      within(canvas.getAllByRole("row")[4]).getByText("VS Code"),
    ).toBeVisible();
    await expect(canvas.getByText("$1.31")).toBeVisible();

    await userEvent.click(canvas.getByRole("radio", { name: "Week" }));
    await expect(canvas.getByRole("radio", { name: "Week" })).toBeChecked();
    await expect(canvas.getByText("2.76M")).toBeVisible();
    await expect(canvas.getByText("$9.17")).toBeVisible();
    await userEvent.click(canvas.getByRole("radio", { name: "Day" }));
    await expect(canvas.getByRole("radio", { name: "Day" })).toBeChecked();

    await userEvent.click(
      canvas.getByRole("button", { name: "gpt-5.6-sol VS Code" }),
    );
    const details = canvas.getByRole("region", {
      name: "gpt-5.6-sol, VS Code interactions",
    });
    await expect(
      within(details).queryByText("Metadata only · Newest first"),
    ).not.toBeInTheDocument();
    await expect(
      within(details).queryByText("chat · copilot"),
    ).not.toBeInTheDocument();
    await expect(within(details).getByText("363,813")).toBeVisible();
    await expect(within(details).getByText("364,154")).toBeVisible();
    await expect(within(details).getByText("$0.18")).toBeVisible();
    await userEvent.click(within(details).getByText("Load older"));
    await expect(within(details).getByText("429 Error")).toBeVisible();
    await expect(
      within(details).queryByText("Load older"),
    ).not.toBeInTheDocument();
    await userEvent.click(
      within(details).getByRole("button", {
        name: "Close interaction details",
      }),
    );

    await userEvent.selectOptions(
      canvas.getByRole("combobox", { name: "Filter usage by model" }),
      "gpt-5.6-sol",
    );
    const modelRow = canvas.getAllByRole("row")[1];
    await expect(within(modelRow).getByText("VS Code")).toBeVisible();
    await expect(
      within(modelRow).queryByText("Claude Code"),
    ).not.toBeInTheDocument();
    await expect(canvas.getByText("$1.31")).toBeVisible();
    await expect(canvas.getByText("394,714")).toBeVisible();
    await expect(canvas.getByText("50")).toBeVisible();

    await userEvent.click(
      canvas.getByRole("button", { name: "Clear filters" }),
    );
    await userEvent.selectOptions(
      canvas.getByRole("combobox", { name: "Filter usage by agent" }),
      "codex_cli_rs",
    );
    const agentRow = canvas.getAllByRole("row")[1];
    await expect(within(agentRow).getByText("Codex")).toBeVisible();
    await expect(
      within(agentRow).queryByText("VS Code"),
    ).not.toBeInTheDocument();

    await userEvent.click(
      canvas.getByRole("button", { name: "Clear filters" }),
    );
    const tokensHeader = canvas.getByRole("columnheader", { name: /Tokens/ });
    await userEvent.click(
      within(tokensHeader).getByRole("button", { name: /Sort by Tokens/ }),
    );
    await expect(tokensHeader).toHaveAttribute("aria-sort", "descending");
    await userEvent.click(
      within(tokensHeader).getByRole("button", { name: /Sort by Tokens/ }),
    );
    await expect(tokensHeader).toHaveAttribute("aria-sort", "ascending");
    const firstUsageRow = canvas.getAllByRole("row")[1];
    await expect(within(firstUsageRow).getByText("gpt-5.6-sol")).toBeVisible();
    await expect(
      within(canvas.getByRole("columnheader", { name: /Calls/ })).getByRole(
        "button",
        { name: "Sort by Calls, descending" },
      ),
    ).toBeVisible();

    await userEvent.click(
      within(
        canvas.getByRole("columnheader", { name: /Estimated cost/ }),
      ).getByRole("button", { name: /Sort by Estimated cost/ }),
    );
  },
};

export const SelectRange: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(canvas.getByRole("radio", { name: "Week" }));
    await expect(args.onRangeChange).toHaveBeenCalledWith("week");
  },
};

export const Interactions: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: "gpt-5.6-sol VS Code" }),
    );
    await expect(
      canvas.getByRole("region", {
        name: "gpt-5.6-sol, VS Code interactions",
      }),
    ).toBeVisible();
  },
};

export const LoadingRange: Story = {
  args: {
    range: "week",
    loadedRange: "day",
    isRangeLoading: true,
  },
  play: async ({ canvas }) => {
    await expect(canvas.getByText("Updating to past 7 days…")).toBeVisible();
    await expect(
      canvas.getByRole("region", { name: "Gateway usage" }),
    ).toHaveAttribute("aria-busy", "true");
    const week = canvas.getByRole("radio", { name: "Week" });
    await expect(week).toBeDisabled();
    await expect(week).toHaveStyle({ cursor: "default" });
  },
};

export const LargeValues: Story = {
  args: {
    llmUsage: {
      ...llmUsage,
      requests: 2_500_000,
      totalTokens: 1_234_567,
      breakdown: [
        {
          model: "gpt-5.6-sol",
          agent: "GitHubCopilotChat",
          requests: 2_500_000,
          totalTokens: 1_234_567,
          estimatedCostUsd: 0.42,
        },
      ],
    },
  },
  play: async ({ canvas }) => {
    await expect(canvas.getAllByText("1.23M")).toHaveLength(2);
    await expect(canvas.getAllByTitle("1,234,567")).toHaveLength(2);
    await expect(canvas.getAllByText("2.5M")).toHaveLength(2);
    await expect(canvas.getAllByTitle("2,500,000")).toHaveLength(2);
  },
};

export const Unavailable: Story = {
  args: { llmUsage: null },
  play: async ({ canvas }) => {
    await expect(canvas.getByText("No usage data")).toBeVisible();
  },
};
