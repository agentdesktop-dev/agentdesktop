import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";

import { DesktopStoryFrame } from "../stories/DesktopStoryFrame";
import {
  emptyDiscovery,
  populatedAccessReport,
  populatedDiscovery,
  unavailableAccessReport,
} from "../stories/fixtures";
import { ToolsView } from "./ToolsView";

const limitedAccessReport = {
  ...populatedAccessReport,
  agents: populatedAccessReport.agents.map((agent) => ({
    ...agent,
    findings: [],
  })),
};

const manyObservationsReport = {
  ...limitedAccessReport,
  agents: limitedAccessReport.agents.map((agent) =>
    agent.kind === "vscode"
      ? {
          ...agent,
          observations: Array.from({ length: 6 }, (_, index) => ({
            category: "filesystem" as const,
            resource: `/workspace/file-${index}.ts`,
            operation: "read" as const,
            count: index + 1,
            evidenceUpdatedAtUnixMs: 1788134400000 + index,
            confidence: "high" as const,
            source: { kind: "history" as const },
          })),
        }
      : agent,
  ),
};

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
    accessLoaded: true,
    accessLoading: false,
    accessReport: populatedAccessReport,
    accessStale: false,
    discovery: populatedDiscovery,
    onOpenAccessSource: fn(),
    unavailable: false,
  },
} satisfies Meta<typeof ToolsView>;

export default meta;
type Story = StoryObj<typeof meta>;

function inventoryFor(canvasElement: HTMLElement, name: string) {
  const inventory = [
    ...canvasElement.querySelectorAll(".tool-inventory-item"),
  ].find(
    (item) => item.querySelector(".tool-cell strong")?.textContent === name,
  );
  if (!(inventory instanceof HTMLDetailsElement)) {
    throw new Error(`${name} inventory is missing`);
  }
  return inventory;
}

function inventorySummary(inventory: HTMLDetailsElement) {
  const summary = inventory.querySelector("summary");
  if (!(summary instanceof HTMLElement)) {
    throw new Error("Agent inventory summary is missing");
  }
  return summary;
}

function accessCategory(container: HTMLElement, name: string) {
  const title = within(container).getByText(name, {
    selector: ".access-category-title",
  });
  const category = title.closest("details");
  if (!(category instanceof HTMLDetailsElement)) {
    throw new Error(`${name} access category is missing`);
  }
  return category;
}

export const Populated: Story = {};

export const Interactions: Story = {
  play: async ({ args, canvas, canvasElement }) => {
    await expect(canvas.getByText("Ollama")).toBeVisible();
    await expect(canvas.getByText("qwen3:8b")).toBeVisible();
    await expect(canvas.getByText("3 access issues need review")).toBeVisible();
    await expect(
      canvas.queryByText("5 agents have coverage limits."),
    ).not.toBeInTheDocument();
    await userEvent.click(
      canvas.getByRole("button", { name: /OpenCode.*1 critical issue/ }),
    );
    const openCodeInventory = inventoryFor(canvasElement, "OpenCode");
    await expect(openCodeInventory).toHaveAttribute("open");
    await expect(
      openCodeInventory.querySelector('[role="tab"][aria-selected="true"]'),
    ).toHaveTextContent("Access");
    await expect(
      canvas.getByText("Uncontained command execution"),
    ).toBeVisible();
    const review = within(openCodeInventory)
      .getByText("Uncontained command execution")
      .closest(".access-review");
    if (!(review instanceof HTMLElement)) {
      throw new Error("Access review section is missing");
    }
    await expect(
      canvasElement.ownerDocument.defaultView?.getComputedStyle(review)
        .paddingBottom,
    ).toBe("12px");
    await expect(
      within(openCodeInventory).queryByText("What was checked"),
    ).not.toBeInTheDocument();
    const openCodeCommands = accessCategory(openCodeInventory, "Commands");
    await expect(openCodeCommands).not.toHaveAttribute("open");
    await expect(within(openCodeCommands).getByText("1 rule")).toBeVisible();
    await userEvent.click(inventorySummary(openCodeCommands));
    await expect(within(openCodeCommands).getByText("Default")).toBeVisible();

    await userEvent.click(
      canvas.getByRole("button", { name: /VS Code.*1 access issue/ }),
    );
    const vscodeInventory = inventoryFor(canvasElement, "VS Code");
    await expect(
      within(vscodeInventory).queryByText("1 access issue"),
    ).not.toBeInTheDocument();
    const vscodeAccess = within(vscodeInventory).getByRole("tabpanel", {
      name: "Access 1",
    });
    await expect(
      within(vscodeAccess).queryByText("linear"),
    ).not.toBeInTheDocument();
    await expect(
      within(vscodeAccess).queryByText("sentry"),
    ).not.toBeInTheDocument();
    const network = accessCategory(vscodeAccess, "Network");
    await expect(within(network).getByText("8 rules")).toBeVisible();
    await userEvent.click(inventorySummary(network));
    const configurationLink = within(vscodeAccess).getByRole("button", {
      name: "Open configuration",
    });
    await expect(
      within(network).queryByRole("button", { name: /Open settings/ }),
    ).not.toBeInTheDocument();
    await userEvent.click(configurationLink);
    await expect(args.onOpenAccessSource).toHaveBeenCalledWith(
      "/Users/developer/Library/Application Support/Code/User/settings.json",
    );
    await expect(within(network).getByText("4 wildcard domains")).toBeVisible();
    await expect(
      within(network).queryByText("Wildcard rules"),
    ).not.toBeInTheDocument();
    await expect(within(network).getAllByText("Configured")[0]).toBeVisible();
    await expect(within(network).getAllByText("Session")[0]).toBeVisible();
    await expect(within(network).getByText("docs.rs")).toBeVisible();
    await expect(within(network).getByText("localhost")).toBeVisible();
    const commands = accessCategory(vscodeAccess, "Commands");
    await expect(commands).not.toHaveAttribute("open");
    await expect(within(commands).getByText("3 rules")).toBeVisible();
    await userEvent.click(inventorySummary(commands));
    await expect(within(commands).getByText("Configured")).toBeVisible();
    await expect(within(commands).getByText("Session")).toBeVisible();
    await expect(within(commands).getByText("cargo test")).toBeVisible();
    await expect(
      within(commands).getByText("recorded terminal commands"),
    ).toBeVisible();
    const filesystem = accessCategory(vscodeAccess, "Filesystem");
    await expect(within(filesystem).getByText("Default")).toBeVisible();
    await expect(within(filesystem).getByText("Session")).toBeVisible();
    await expect(
      within(filesystem).getByText(
        /frontend\/desktop\/src\/components\/AgentToolInventory\.tsx/,
      ),
    ).toBeVisible();
    within(vscodeInventory).getByRole("tab", { name: "Access 1" }).focus();
    await userEvent.keyboard("{ArrowRight}");
    await expect(
      within(vscodeInventory).getByRole("tab", { name: "MCP servers 8" }),
    ).toHaveAttribute("aria-selected", "true");
    await userEvent.keyboard("{ArrowRight}");
    await expect(
      await within(vscodeInventory).findByRole("tabpanel", {
        name: "Skills 12",
      }),
    ).toBeVisible();
    await expect(
      within(vscodeInventory).getByRole("tab", { name: "Skills 12" }),
    ).toHaveAttribute("aria-selected", "true");
    await userEvent.click(
      within(vscodeInventory).getByRole("button", {
        name: "Next Skills page",
      }),
    );
    await expect(canvas.getByText("Performance diagnosis")).toBeVisible();
  },
};

export const MultipleConfigurations: Story = {
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /Claude Code.*1 access issue/ }),
    );
    const claudeInventory = inventoryFor(canvasElement, "Claude Code");
    const configurations = claudeInventory.querySelector(
      ".access-configurations",
    );
    if (!(configurations instanceof HTMLDetailsElement)) {
      throw new Error("Configuration menu is missing");
    }
    await userEvent.click(inventorySummary(configurations));
    await expect(
      within(configurations).getByRole("button", { name: ".claude.json" }),
    ).toBeVisible();
    await expect(
      within(configurations).getByRole("button", { name: "settings.json" }),
    ).toBeVisible();
  },
};

export const FiltersAccessSources: Story = {
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /VS Code.*1 access issue/ }),
    );
    const vscodeInventory = inventoryFor(canvasElement, "VS Code");
    const vscodeAccess = within(vscodeInventory).getByRole("tabpanel", {
      name: "Access 1",
    });
    const network = accessCategory(vscodeAccess, "Network");
    await userEvent.click(inventorySummary(network));

    await expect(
      [...network.querySelectorAll(".access-origin-badge")].map((badge) =>
        badge.textContent?.trim(),
      ),
    ).toEqual(["Default", "Configured", "Configured", "Session", "Session"]);

    const filterSummary = vscodeAccess.querySelector(
      ".access-filter > summary",
    );
    if (!(filterSummary instanceof HTMLElement)) {
      throw new Error("Access source filter is missing");
    }
    await expect(filterSummary).toHaveAccessibleName(
      "Filter access sources: All sources",
    );
    await userEvent.click(filterSummary);
    const sessionFilter = within(vscodeAccess).getByRole("checkbox", {
      name: "Session",
    });
    await userEvent.click(sessionFilter);
    await expect(sessionFilter).not.toBeChecked();
    await expect(within(network).getByText("6 of 8 rules")).toBeVisible();
    await expect(
      within(network).queryByText("docs.rs"),
    ).not.toBeInTheDocument();

    await userEvent.click(sessionFilter);
    await expect(sessionFilter).toBeChecked();
    await expect(within(network).getByText("docs.rs")).toBeVisible();
  },
};

export const Empty: Story = {
  args: { discovery: emptyDiscovery },
};

export const Unavailable: Story = {
  args: { discovery: null, unavailable: true },
};

export const CheckingAccess: Story = {
  args: {
    accessLoaded: false,
    accessLoading: true,
    accessReport: null,
  },
  play: async ({ canvasElement }) => {
    const overview = canvasElement.querySelector(".tools-audit-overview");
    if (!(overview instanceof HTMLElement)) {
      throw new Error("Access audit overview is missing");
    }
    await expect(
      within(overview).getByText("Checking local access"),
    ).toBeVisible();
    const vscodeInventory = inventoryFor(canvasElement, "VS Code");
    await userEvent.click(inventorySummary(vscodeInventory));
    await expect(
      within(vscodeInventory).getByText("Checking local access"),
    ).toBeVisible();
  },
};

export const AccessUnavailable: Story = {
  args: {
    accessLoaded: true,
    accessReport: unavailableAccessReport,
  },
  play: async ({ canvasElement }) => {
    const overview = canvasElement.querySelector(".tools-audit-overview");
    if (!(overview instanceof HTMLElement)) {
      throw new Error("Access audit overview is missing");
    }
    await expect(
      within(overview).getByText("Local access audit unavailable"),
    ).toBeVisible();
    const vscodeInventory = inventoryFor(canvasElement, "VS Code");
    await userEvent.click(inventorySummary(vscodeInventory));
    await expect(
      within(vscodeInventory).getByText(
        /operating-system user could not be identified/,
      ),
    ).toBeVisible();
  },
};

export const StaleAccess: Story = {
  args: { accessStale: true },
  play: async ({ canvas }) => {
    await expect(canvas.getByText(/Last successful check/)).toBeVisible();
  },
};

export const LimitedAccess: Story = {
  args: { accessReport: limitedAccessReport },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByText("No risks found in checked sources"),
    ).toBeVisible();
    await expect(
      canvas.queryByText("Limited visibility"),
    ).not.toBeInTheDocument();
    await expect(
      canvas.queryByText("Coverage incomplete"),
    ).not.toBeInTheDocument();
  },
};

export const ManyObservations: Story = {
  args: { accessReport: manyObservationsReport },
  play: async ({ canvasElement }) => {
    const vscodeInventory = inventoryFor(canvasElement, "VS Code");
    await userEvent.click(inventorySummary(vscodeInventory));
    await expect(
      within(vscodeInventory).queryByText("/workspace/file-0.ts"),
    ).not.toBeInTheDocument();
    const filesystem = accessCategory(vscodeInventory, "Filesystem");
    await expect(within(filesystem).getByText("7 paths")).toBeVisible();
    await userEvent.click(inventorySummary(filesystem));
    await expect(
      within(vscodeInventory).getByText("/workspace/file-0.ts"),
    ).toBeVisible();
  },
};

export const ReflowAt320: Story = {
  args: { discovery: populatedDiscovery },
  globals: {
    viewport: { value: "reflow", isRotated: false },
  },
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /VS Code.*1 access issue/ }),
    );
    const vscodeInventory = inventoryFor(canvasElement, "VS Code");
    await expect(
      await within(vscodeInventory).findByRole("tabpanel", {
        name: "Access 1",
      }),
    ).toBeVisible();
    const documentElement = canvasElement.ownerDocument.documentElement;
    const viewportWidth = canvasElement.ownerDocument.defaultView?.innerWidth;
    await expect(documentElement.scrollWidth).toBeLessThanOrEqual(
      viewportWidth ?? 0,
    );
  },
};
