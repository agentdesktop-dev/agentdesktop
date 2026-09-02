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
            resource: `/workspace/project-${index}`,
            operations: ["read" as const, "write" as const],
            workspace: `/workspace/project-${index}`,
            count: (index + 1) * 10,
            sessionCount: index + 1,
            resourceCount: (index + 1) * 4,
            workspaceCount: 1,
            evidenceUpdatedAtUnixMs: 1788134400000 + index,
            confidence: "high" as const,
            source: { kind: "history" as const },
          })),
        }
      : agent,
  ),
};

const claudeHardeningReport = {
  ...populatedAccessReport,
  agents: populatedAccessReport.agents.map((agent) =>
    agent.kind === "claude-code"
      ? {
          ...agent,
          findings: [
            {
              severity: "warning" as const,
              title: "Shell sandbox not configured",
              detail:
                "Claude shell subprocesses are approval-gated but no configured OS sandbox boundary was found.",
              category: "execution" as const,
            },
          ],
        }
      : agent,
  ),
};

const configuredReadOnlyNetworkReport = {
  ...populatedAccessReport,
  agents: populatedAccessReport.agents.map((agent) =>
    agent.kind === "vscode"
      ? {
          ...agent,
          capabilities: [
            ...(agent.capabilities ?? []),
            {
              category: "network" as const,
              resource: "legacy.example.com",
              operations: ["connect" as const],
              decision: "allow" as const,
              enforcement: "harness" as const,
              source: {
                kind: "configuration" as const,
                path: "/Users/developer/Library/Application Support/Code/User/settings.json",
              },
              detail:
                "Advanced: request and response approvals are configured separately. Edit this rule in the configuration file",
            },
          ],
        }
      : agent,
  ),
};

const allConfiguredNetworkRulesReadOnlyReport = {
  ...populatedAccessReport,
  agents: populatedAccessReport.agents.map((agent) =>
    agent.kind === "vscode"
      ? {
          ...agent,
          capabilities: (agent.capabilities ?? []).map((capability) =>
            capability.category === "network" &&
            capability.source.kind === "configuration"
              ? {
                  ...capability,
                  rule: undefined,
                  detail:
                    "Advanced: request and response approvals are configured separately. Edit this rule in the configuration file",
                }
              : capability,
          ),
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
    allowAccessEditing: true,
    discovery: populatedDiscovery,
    onApplyNetworkRuleChange: fn(async () => {}),
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

export const ManagedAccess: Story = {
  args: {
    accessReport: claudeHardeningReport,
    allowAccessEditing: false,
  },
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /Claude Code.*1 access issue/ }),
    );
    const claudeInventory = inventoryFor(canvasElement, "Claude Code");
    await expect(
      within(claudeInventory).queryByRole("tab", { name: "Policy" }),
    ).not.toBeInTheDocument();
    await expect(
      within(claudeInventory).queryByText("Access controls"),
    ).not.toBeInTheDocument();
  },
};

export const ShowsClaudeHardeningFindingWithoutAction: Story = {
  args: { accessReport: claudeHardeningReport },
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /Claude Code.*1 access issue/ }),
    );
    const claudeInventory = inventoryFor(canvasElement, "Claude Code");
    const access = within(claudeInventory).getByRole("tabpanel", {
      name: "Access 1",
    });
    await expect(
      within(claudeInventory).queryByRole("tab", { name: "Policy" }),
    ).not.toBeInTheDocument();
    await expect(
      within(access).getByText("Shell sandbox not configured"),
    ).toBeVisible();
    await expect(
      within(access).queryByRole("button", { name: "Use recommended" }),
    ).not.toBeInTheDocument();
    await expect(
      within(access).getByText("Configured destinations"),
    ).toBeVisible();
  },
};

export const ShowsReadOnlyConfiguredDomain: Story = {
  args: { accessReport: configuredReadOnlyNetworkReport },
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /VS Code.*1 access issue/ }),
    );
    const inventory = inventoryFor(canvasElement, "VS Code");
    const access = within(inventory).getByRole("tabpanel", {
      name: "Access 1",
    });
    const network = accessCategory(access, "Network");
    await expect(within(network).getByText("9 rules")).toBeVisible();
    await expect(within(network).getByText("5 editable")).toBeVisible();
    const configuredDomain = within(network).getByText("legacy.example.com");
    await expect(configuredDomain).toBeVisible();
    const row = configuredDomain.closest(".access-setting-row");
    if (!(row instanceof HTMLElement)) {
      throw new Error("Configured read-only domain row is missing");
    }
    await expect(within(row).getByText("Configured")).toBeVisible();
    await expect(
      within(row).getByText("Custom request and response approvals"),
    ).toBeVisible();
    await expect(
      within(row).queryByText("Can connect without asking"),
    ).not.toBeInTheDocument();
    await expect(within(row).getByText("Advanced")).toBeVisible();
    await expect(
      within(row).getByText(/Edit this rule in the configuration file/),
    ).toBeVisible();
    await expect(
      within(network).queryByRole("group", {
        name: "Access for legacy.example.com",
      }),
    ).not.toBeInTheDocument();
  },
};

export const ExplainsAllReadOnlyConfiguredDomains: Story = {
  args: { accessReport: allConfiguredNetworkRulesReadOnlyReport },
  play: async ({ canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /VS Code.*1 access issue/ }),
    );
    const inventory = inventoryFor(canvasElement, "VS Code");
    const access = within(inventory).getByRole("tabpanel", {
      name: "Access 1",
    });
    const network = accessCategory(access, "Network");
    await expect(within(network).getByText("0 editable")).toBeVisible();
    await expect(
      within(network).getByText(
        "5 configured destinations are Advanced and read-only here",
      ),
    ).toBeVisible();
    await expect(
      within(network).getAllByText(/Edit this rule in the configuration file/),
    ).toHaveLength(2);
    await expect(
      within(network).queryByText("No editable destinations configured."),
    ).not.toBeInTheDocument();
  },
};

export const EditsVscodeNetworkRules: Story = {
  play: async ({ args, canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /VS Code.*1 access issue/ }),
    );
    const inventory = inventoryFor(canvasElement, "VS Code");
    const access = within(inventory).getByRole("tabpanel", {
      name: "Access 1",
    });
    const approval = within(access).getByRole("group", {
      name: "Access for *.githubusercontent.com",
    });
    await userEvent.click(
      within(approval).getByRole("button", { name: "Ask" }),
    );
    await userEvent.click(
      within(access).getByRole("button", { name: "Remove *.amazon.com" }),
    );
    await userEvent.type(
      within(access).getByRole("textbox", { name: "New destination" }),
      "api.internal.example",
    );
    await userEvent.click(
      within(access).getByRole("button", { name: "Add as Ask first" }),
    );
    await expect(args.onApplyNetworkRuleChange).not.toHaveBeenCalled();
    await expect(within(access).getByText("3 pending")).toBeVisible();
    await userEvent.click(
      within(access).getByRole("button", { name: "Apply changes" }),
    );
    await expect(args.onApplyNetworkRuleChange).toHaveBeenCalledWith({
      agentKind: "vscode",
      operation: "setDecision",
      ruleId: "vscode-url-githubusercontent",
      decision: "ask",
    });
    await expect(args.onApplyNetworkRuleChange).toHaveBeenCalledWith({
      agentKind: "vscode",
      operation: "remove",
      ruleId: "vscode-url-amazon",
    });
    await expect(args.onApplyNetworkRuleChange).toHaveBeenCalledWith({
      agentKind: "vscode",
      operation: "add",
      resource: "api.internal.example",
      decision: "ask",
    });
    await expect(args.onApplyNetworkRuleChange).toHaveBeenCalledTimes(3);
  },
};

export const EditsClaudeNetworkRules: Story = {
  play: async ({ args, canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /Claude Code.*1 access issue/ }),
    );
    const inventory = inventoryFor(canvasElement, "Claude Code");
    const access = within(inventory).getByRole("tabpanel", {
      name: "Access 1",
    });
    const decision = within(access).getByRole("group", {
      name: "Access for *.github.com",
    });
    await expect(
      within(decision).queryByRole("button", { name: "Ask" }),
    ).not.toBeInTheDocument();
    await userEvent.click(
      within(decision).getByRole("button", { name: "Block" }),
    );
    await expect(args.onApplyNetworkRuleChange).not.toHaveBeenCalled();
    await userEvent.click(
      within(access).getByRole("button", { name: "Apply changes" }),
    );
    await expect(args.onApplyNetworkRuleChange).toHaveBeenCalledWith({
      agentKind: "claude-code",
      operation: "setDecision",
      ruleId: "claude-sandbox-github",
      decision: "deny",
    });
  },
};

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
    await expect(
      within(openCodeInventory).queryByText("Recommendations"),
    ).not.toBeInTheDocument();
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
      within(vscodeAccess).queryByText("Recommendations"),
    ).not.toBeInTheDocument();
    await expect(
      within(vscodeAccess).queryByText("linear"),
    ).not.toBeInTheDocument();
    await expect(
      within(vscodeAccess).queryByText("sentry"),
    ).not.toBeInTheDocument();
    await expect(within(vscodeAccess).getByText("5 editable")).toBeVisible();
    await expect(
      within(vscodeAccess).getAllByText("All subdomains"),
    ).toHaveLength(4);
    const network = accessCategory(vscodeAccess, "Network");
    await expect(within(network).getByText("8 rules")).toBeVisible();
    if (!network.open) await userEvent.click(inventorySummary(network));
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
    await expect(
      within(network).queryByText("Wildcard rules"),
    ).not.toBeInTheDocument();
    await expect(
      within(network).queryByText("Configured"),
    ).not.toBeInTheDocument();
    await expect(within(network).getAllByText("Session")[0]).toBeVisible();
    await expect(within(network).getByText("docs.rs")).toBeVisible();
    await expect(within(network).getByText("localhost")).toBeVisible();
    const commands = accessCategory(vscodeAccess, "Commands");
    await expect(commands).not.toHaveAttribute("open");
    await expect(within(commands).getByText("4 rules")).toBeVisible();
    await userEvent.click(inventorySummary(commands));
    await expect(within(commands).getByText("Configured")).toBeVisible();
    await expect(within(commands).getAllByText("Session")[0]).toBeVisible();
    await expect(within(commands).getByText("cargo test")).toBeVisible();
    await expect(
      within(commands).getByText("recorded terminal commands"),
    ).toBeVisible();
    await expect(within(commands).getAllByText("cd")).toHaveLength(1);
    await expect(
      within(commands).getByText(
        "18 runs across 7 sessions · 3 workspaces · latest Aug 31, 2026",
      ),
    ).toBeVisible();
    const filesystem = accessCategory(vscodeAccess, "Filesystem");
    await expect(within(filesystem).getByText("2 paths")).toBeVisible();
    await expect(within(filesystem).getByText("Default")).toBeVisible();
    await expect(within(filesystem).getByText("Session")).toBeVisible();
    await expect(
      within(filesystem).getAllByText("active workspace"),
    ).toHaveLength(1);
    await expect(
      within(filesystem).getByText("Read allowed · Write requires approval"),
    ).toBeVisible();
    await expect(
      within(filesystem).getByText("/Users/developer/projects/agentdesktop"),
    ).toBeVisible();
    await expect(
      within(filesystem).getByText(
        "42 read/write accesses across 8 sessions · 31 paths · latest Aug 31, 2026",
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
    if (!network.open) await userEvent.click(inventorySummary(network));

    await expect(
      [...network.querySelectorAll(".access-origin-badge")].map((badge) =>
        badge.textContent?.trim(),
      ),
    ).toEqual(["Default", "Session", "Session"]);

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
      within(vscodeInventory).queryByText("/workspace/project-0"),
    ).not.toBeInTheDocument();
    const filesystem = accessCategory(vscodeInventory, "Filesystem");
    await expect(within(filesystem).getByText("7 paths")).toBeVisible();
    await userEvent.click(inventorySummary(filesystem));
    await expect(
      within(vscodeInventory).getByText("/workspace/project-0"),
    ).toBeVisible();
  },
};

export const ReflowAt320: Story = {
  args: { discovery: populatedDiscovery },
  globals: {
    viewport: { value: "reflow", isRotated: false },
  },
  play: async ({ args, canvas, canvasElement }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: /VS Code.*1 access issue/ }),
    );
    const vscodeInventory = inventoryFor(canvasElement, "VS Code");
    const access = await within(vscodeInventory).findByRole("tabpanel", {
      name: "Access 1",
    });
    const decision = within(access).getByRole("group", {
      name: "Access for *.githubusercontent.com",
    });
    await userEvent.click(
      within(decision).getByRole("button", { name: "Ask" }),
    );
    await expect(within(access).getByText("1 pending")).toBeVisible();
    await expect(
      within(access).getByRole("button", { name: "Apply changes" }),
    ).toBeVisible();
    await expect(args.onApplyNetworkRuleChange).not.toHaveBeenCalled();
    const documentElement = canvasElement.ownerDocument.documentElement;
    const viewportWidth = canvasElement.ownerDocument.defaultView?.innerWidth;
    await expect(documentElement.scrollWidth).toBeLessThanOrEqual(
      viewportWidth ?? 0,
    );
  },
};
