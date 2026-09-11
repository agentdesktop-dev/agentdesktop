import { friendlyTool, ToolIcon, ToolInventory } from "@agentdesktop/ui";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Code2 } from "lucide-react";
import { expect, within } from "storybook/test";

const meta = {
  title: "Shared/Tool presentation",
  parameters: { layout: "padded" },
  tags: ["test"],
} satisfies Meta;

export default meta;
type Story = StoryObj<typeof meta>;

// Independent expectations: these are presentation identities, not builder choices.
const knownTools = [
  { id: "claude-code", aliases: ["claude_code"], label: "Claude Code" },
  {
    id: "claude-desktop",
    aliases: ["claude_desktop"],
    label: "Claude Desktop",
  },
  { id: "codex", aliases: [], label: "Codex" },
  { id: "opencode", aliases: [], label: "OpenCode" },
  { id: "vscode", aliases: [], label: "VS Code" },
];

const knownCases = knownTools.flatMap((tool) =>
  [tool.id, ...tool.aliases].flatMap((kind) =>
    [
      kind,
      kind.toUpperCase(),
      kind.charAt(0).toUpperCase() + kind.slice(1),
    ].map((variant) => ({ ...tool, kind: variant })),
  ),
);

export const IdentitiesAndAliases: Story = {
  render: () => (
    <main>
      <h1>Known inventory tools</h1>
      <div className="stack">
        {knownTools.map((tool) => (
          <div key={tool.id} data-testid={`inventory-${tool.id}`}>
            <ToolInventory
              discovery={{ kind: tool.id, path: `/example/${tool.id}` }}
            />
          </div>
        ))}
      </div>
      <h2>Identities, aliases and case</h2>
      <ul>
        {knownCases.map(({ kind }) => (
          <li key={kind} data-testid={`case-${kind}`}>
            <code>{kind}</code>
            <span className="tool-cell">
              <ToolIcon kind={kind} />
              <span>{friendlyTool(kind)}</span>
            </span>
          </li>
        ))}
      </ul>
    </main>
  ),
  play: async ({ canvas, canvasElement }) => {
    await expect(
      canvasElement.querySelectorAll(".tool-inventory-item"),
    ).toHaveLength(5);
    const sources = new Set<string>();
    for (const tool of knownTools) {
      const inventory = canvas.getByTestId(`inventory-${tool.id}`);
      await expect(within(inventory).getByText(tool.label)).toBeVisible();
      const source = inventory
        .querySelector("img.tool-icon")
        ?.getAttribute("src");
      await expect(source).toBeTruthy();
      if (source) sources.add(source);
      for (const { kind, label } of knownCases.filter(
        (candidate) => candidate.id === tool.id,
      )) {
        await expect(friendlyTool(kind)).toBe(label);
        const row = canvas.getByTestId(`case-${kind}`);
        await expect(
          within(row).getByText(label, { selector: "span" }),
        ).toBeVisible();
        const icon = row.querySelector("img.tool-icon");
        await expect(icon).toHaveAttribute("src", source);
        await expect(icon).toHaveAttribute("alt", "");
        await expect(icon).toHaveAttribute("aria-hidden", "true");
        await expect(row.querySelector(".tool-icon-fallback")).toBeNull();
      }
    }
    await expect(sources.size).toBe(5);
  },
};

const unknownKinds = [
  "Future Tool",
  "",
  "toString",
  "__proto__",
  "constructor",
  "hasOwnProperty",
  "valueOf",
  "claudeCode",
  "claudeDesktop",
  "open_code",
  "open-code",
  "vs_code",
  "vs-code",
  "claude code",
  "claude-code ",
  "copilot",
];

export const UnknownIdentities: Story = {
  render: () => (
    <main>
      <h1>Unknown tools</h1>
      <p>
        Unknown names are preserved and use this fallback icon:{" "}
        <Code2 data-testid="fallback-icon" size={16} aria-hidden="true" />
      </p>
      <ul>
        {unknownKinds.map((kind) => (
          <li key={kind} data-testid={`unknown-${encodeURIComponent(kind)}`}>
            <code>{kind || "(empty)"}</code>
            <span className="tool-cell">
              <ToolIcon kind={kind} />
              <span>{friendlyTool(kind)}</span>
            </span>
          </li>
        ))}
      </ul>
    </main>
  ),
  play: async ({ canvas }) => {
    const fallback = canvas.getByTestId("fallback-icon");
    for (const kind of unknownKinds) {
      await expect(friendlyTool(kind)).toBe(kind);
      const row = canvas.getByTestId(`unknown-${encodeURIComponent(kind)}`);
      await expect(row.querySelector(".tool-cell")?.textContent).toBe(kind);
      await expect(row.querySelector("img")).toBeNull();
      const icon = row.querySelector("svg.tool-icon-fallback");
      await expect(icon).toBeVisible();
      await expect(icon?.innerHTML).toBe(fallback.innerHTML);
      await expect(icon).toHaveAttribute("width", "16");
      await expect(icon).toHaveAttribute("aria-hidden", "true");
    }
  },
};
