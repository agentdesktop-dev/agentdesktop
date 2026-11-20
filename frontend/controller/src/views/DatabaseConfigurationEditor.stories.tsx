import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fireEvent, fn, userEvent } from "storybook/test";
import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import type {
  ActiveDaemonConfig,
  DaemonConfigAuthor,
  DaemonConfigRevisionSummary,
} from "../types";
import { DatabaseConfigurationEditor } from "./DatabaseConfigurationEditor";

const yaml = [
  "# Keep this comment when using the builder.",
  "customSettings:",
  "  owner: platform",
  "programs:",
  "  claudeCode: {}",
].join("\n");

const author: DaemonConfigAuthor = {
  kind: "human",
  issuer: "https://issuer.example",
  subject: "admin@example.com",
  client_id: null,
  display: "Admin",
};

const active: ActiveDaemonConfig = {
  config: { programs: { claudeCode: {} } },
  yaml,
  revision: 7,
  sha256: "a".repeat(64),
  source: "database",
  writable: true,
  created_at: 1_700_000_000,
  author,
  message: "Initial fleet setup",
  reload_error: null,
};

const history: DaemonConfigRevisionSummary[] = [
  {
    revision: 7,
    sha256: "a".repeat(64),
    created_at: 1_700_000_000,
    author,
    message: "Initial fleet setup",
  },
];

const meta = {
  title: "Controller/Database Configuration",
  component: DatabaseConfigurationEditor,
  decorators: [
    (Story) => (
      <ControllerStoryFrame path="/configuration">
        <Story />
      </ControllerStoryFrame>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  args: {
    active,
    devices: [],
    devicesLoading: false,
    devicesError: null,
    canUpdate: true,
    history,
    historyError: null,
    historyLoading: false,
    historyHasMore: false,
    selectedRevision: null,
    selectedRevisionData: null,
    selectedRevisionError: null,
    selectedRevisionLoading: false,
    previousRevisionData: null,
    previousRevisionError: null,
    previousRevisionLoading: false,
    onLoadMore: fn(),
    onSelectRevision: fn(),
    onSave: fn(async ({ yaml: nextYaml, base_revision }) => ({
      ...active,
      yaml: nextYaml,
      revision: base_revision + 1,
    })),
    onLoadLatest: fn(async () => active),
    onReauthenticate: fn(),
    onSaved: fn(),
  },
} satisfies Meta<typeof DatabaseConfigurationEditor>;

export default meta;
type Story = StoryObj<typeof meta>;

export const EditsAndSaves: Story = {
  play: async ({ args, canvas }) => {
    const yamlEditor = canvas.getByRole("textbox", {
      name: "Daemon configuration YAML",
    });
    fireEvent.change(yamlEditor, {
      target: { value: "programs:\n  claudeCode: {}\n" },
    });
    await userEvent.click(
      canvas.getByRole("button", { name: "Review changes" }),
    );
    await expect(
      canvas.getByRole("heading", { name: "Review before saving" }),
    ).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Save configuration" }),
    );
    await expect(args.onSave).toHaveBeenCalledWith({
      base_revision: 7,
      yaml: "programs:\n  claudeCode: {}\n",
    });
    await expect(canvas.getByText("Revision 8 saved")).toBeVisible();
  },
};

export const BuilderPreservesUnmanagedYaml: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByRole("tab", { name: "Builder" }));
    await userEvent.click(
      canvas.getByRole("button", { name: "Apply to draft" }),
    );
    const value = canvas.getByRole("textbox", {
      name: "Daemon configuration YAML",
    });
    const editedYaml = (value as HTMLTextAreaElement).value;
    await expect(editedYaml).toContain(
      "# Keep this comment when using the builder.",
    );
    await expect(editedYaml).toContain("customSettings:\n  owner: platform");
  },
};
