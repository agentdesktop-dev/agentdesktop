import { SYSTEM_THEME_QUERY } from "@agentdesktop/ui";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";

import { DesktopStoryFrame } from "../stories/DesktopStoryFrame";
import {
  allProgramsManaged,
  bootstrap,
  noConfiguredPrograms,
  noProgramsManaged,
  organizationManagedPrograms,
  selectedProgramsManaged,
} from "../stories/fixtures";
import { SettingsView } from "./SettingsView";

const meta = {
  title: "Desktop/Settings",
  component: SettingsView,
  decorators: [
    (Story) => (
      <DesktopStoryFrame pageTitle="Settings" view="settings">
        <Story />
      </DesktopStoryFrame>
    ),
  ],
  tags: ["test"],
  args: {
    settings: bootstrap.settings,
    disabled: false,
    onStartupChange: fn(),
    programManagement: allProgramsManaged,
    programsLoading: false,
    savingPrograms: false,
    onProgramChoiceChange: fn(),
  },
} satisfies Meta<typeof SettingsView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const System: Story = {
  play: async ({ canvas, canvasElement }) => {
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toHaveValue("system");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      window.matchMedia(SYSTEM_THEME_QUERY).matches ? "dark" : "light",
    );
  },
};

export const ChangesColorMode: Story = {
  play: async ({ canvas, canvasElement }) => {
    const select = canvas.getByRole("combobox", { name: "Color mode" });
    const root = canvasElement.ownerDocument.documentElement;
    for (const mode of ["dark", "light", "system"]) {
      await userEvent.selectOptions(select, mode);
      const theme =
        mode === "system"
          ? window.matchMedia(SYSTEM_THEME_QUERY).matches
            ? "dark"
            : "light"
          : mode;
      await expect(select).toHaveValue(mode);
      await expect(root).toHaveAttribute("data-theme", theme);
      await expect(root).toHaveStyle({ colorScheme: theme });
    }
  },
};

export const ChangesStartupPreference: Story = {
  play: async ({ args, canvas }) => {
    await userEvent.click(
      canvas.getByRole("checkbox", { name: "Open window at startup" }),
    );
    await expect(args.onStartupChange).toHaveBeenCalledWith(false);
  },
};

export const Saving: Story = {
  args: { disabled: true },
  play: async ({ canvas }) => {
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toBeDisabled();
    await expect(
      canvas.getByRole("checkbox", { name: "Open window at startup" }),
    ).toBeDisabled();
  },
};

export const Dark: Story = {
  globals: { colorMode: "dark" },
};

export const DarkReflow: Story = {
  globals: {
    colorMode: "dark",
    viewport: { value: "reflow", isRotated: false },
  },
};

function managedTools(canvasElement: HTMLElement) {
  const heading = within(canvasElement).getByRole("heading", {
    name: "Managed tools",
  });
  const card = heading.closest("section");
  if (!card) throw new Error("Managed tools card not found");
  return within(card);
}

export const ManagesAllTools: Story = {
  play: async ({ canvasElement }) => {
    const card = managedTools(canvasElement);
    await expect(card.getByRole("radio", { name: "All" })).toBeChecked();
    for (const name of ["Claude Code", "Codex", "VS Code"]) {
      const program = card.getByRole("checkbox", { name });
      await expect(program).toBeChecked();
      await expect(program).toBeDisabled();
    }
    await expect(
      card.getByRole("checkbox", { name: "VS Code" }),
    ).toHaveAccessibleDescription(
      "Conflict settings.json already sets a value Agent Desktop manages",
    );
  },
};

export const ManagesSelectedTools: Story = {
  args: { programManagement: selectedProgramsManaged },
  play: async ({ canvasElement }) => {
    const card = managedTools(canvasElement);
    await expect(card.getByRole("radio", { name: "Only these" })).toBeChecked();
    await expect(
      card.getByRole("checkbox", { name: "Claude Code" }),
    ).toBeChecked();
    const codex = card.getByRole("checkbox", { name: "Codex" });
    await expect(codex).not.toBeChecked();
    await expect(codex).toBeEnabled();
    await expect(codex).toHaveAccessibleDescription("Opted out");
  },
};

export const ManagesNoTools: Story = {
  args: { programManagement: noProgramsManaged },
  play: async ({ canvasElement }) => {
    const card = managedTools(canvasElement);
    await expect(card.getByRole("radio", { name: "None" })).toBeChecked();
    for (const name of ["Claude Code", "Codex", "VS Code"]) {
      await expect(card.getByRole("checkbox", { name })).not.toBeChecked();
    }
    await expect(card.getAllByText("Opted out")).toHaveLength(3);
  },
};

export const ChoosesManagedTools: Story = {
  args: { programManagement: selectedProgramsManaged },
  play: async ({ args, canvasElement }) => {
    const card = managedTools(canvasElement);
    await userEvent.click(card.getByRole("checkbox", { name: "Codex" }));
    await expect(args.onProgramChoiceChange).toHaveBeenLastCalledWith({
      mode: "selected",
      programs: ["claude-code", "codex", "vscode"],
    });
    await userEvent.click(card.getByRole("checkbox", { name: "VS Code" }));
    await expect(args.onProgramChoiceChange).toHaveBeenLastCalledWith({
      mode: "selected",
      programs: ["claude-code"],
    });
    await userEvent.click(card.getByRole("radio", { name: "None" }));
    await expect(args.onProgramChoiceChange).toHaveBeenLastCalledWith({
      mode: "none",
    });
    await userEvent.click(card.getByRole("radio", { name: "All" }));
    await expect(args.onProgramChoiceChange).toHaveBeenLastCalledWith({
      mode: "all",
    });
  },
};

export const ChoosesOnlyTheseFromAll: Story = {
  play: async ({ args, canvasElement }) => {
    const card = managedTools(canvasElement);
    await userEvent.click(card.getByRole("radio", { name: "Only these" }));
    await expect(args.onProgramChoiceChange).toHaveBeenCalledWith({
      mode: "selected",
      programs: ["claude-code", "codex", "vscode"],
    });
  },
};

export const ManagedByOrganization: Story = {
  args: { programManagement: organizationManagedPrograms },
  play: async ({ canvasElement }) => {
    const card = managedTools(canvasElement);
    const list = card.getByRole("list", {
      name: "Managed by your organization",
    });
    await expect(within(list).getAllByRole("listitem")).toHaveLength(3);
    await expect(within(list).getByText("Grok Build")).toBeVisible();
    await expect(within(list).getByText("Pending")).toBeVisible();
    await expect(card.queryByRole("radio")).toBeNull();
    await expect(card.queryByRole("checkbox")).toBeNull();
  },
};

export const SavingManagedTools: Story = {
  args: { programManagement: selectedProgramsManaged, savingPrograms: true },
  play: async ({ canvasElement }) => {
    const card = managedTools(canvasElement);
    await expect(card.getByRole("status")).toHaveTextContent("Saving…");
    for (const radio of card.getAllByRole("radio")) {
      await expect(radio).toBeDisabled();
    }
    for (const checkbox of card.getAllByRole("checkbox")) {
      await expect(checkbox).toBeDisabled();
    }
  },
};

export const NoConfiguredTools: Story = {
  args: { programManagement: noConfiguredPrograms },
  play: async ({ canvasElement }) => {
    const card = managedTools(canvasElement);
    await expect(card.getByText("No tools are configured")).toBeVisible();
    await expect(card.queryByRole("list")).toBeNull();
  },
};

export const ManagedToolsUnavailable: Story = {
  args: { programManagement: null },
  play: async ({ canvasElement }) => {
    const card = managedTools(canvasElement);
    await expect(card.getByText("Managed tools are unavailable")).toBeVisible();
  },
};

export const ManagedToolsDarkReflow: Story = {
  args: { programManagement: selectedProgramsManaged },
  globals: {
    colorMode: "dark",
    viewport: { value: "reflow", isRotated: false },
  },
};
