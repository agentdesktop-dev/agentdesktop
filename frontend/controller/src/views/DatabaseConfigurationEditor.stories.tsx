import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fireEvent, fn, userEvent } from "storybook/test";
import { ApiError } from "../auth";
import { ControllerStoryFrame } from "../stories/ControllerStoryFrame";
import { macDevice } from "../stories/fixtures";
import type {
  ActiveDaemonConfig,
  DaemonConfigAuthor,
  DaemonConfigRevisionSummary,
} from "../types";
import {
  DatabaseConfigurationEditor,
  type DatabaseConfigurationEditorProps,
} from "./DatabaseConfigurationEditor";

type Save = DatabaseConfigurationEditorProps["onSave"];

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
    onSave: fn<Save>(async ({ yaml: nextYaml, base_revision }) => ({
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
    await expect(canvas.getByRole("heading", { name: "Review" })).toBeVisible();
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

export const WarnsAboutOlderDaemonsWithoutBlocking: Story = {
  args: {
    active: { ...active, controller_version: "0.10.0" },
    devices: [
      { ...macDevice, agent_version: "0.9.9", last_seen_at: null },
      { ...macDevice, agent_version: "0.10.0+build" },
      { ...macDevice, agent_version: "0.11.0" },
      { ...macDevice, agent_version: "" },
    ],
  },
  play: async ({ args, canvas }) => {
    fireEvent.change(
      canvas.getByRole("textbox", { name: "Daemon configuration YAML" }),
      { target: { value: "programs: {}\n" } },
    );
    await userEvent.click(
      canvas.getByRole("button", { name: "Review changes" }),
    );
    await expect(
      canvas.getByText(
        "1 enrolled daemon is older than this controller and may not understand the new configuration.",
      ),
    ).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Save configuration" }),
    );
    await expect(args.onSave).toHaveBeenCalledWith({
      base_revision: 7,
      yaml: "programs: {}\n",
    });
  },
};

export const BuilderPreservesUnmanagedYaml: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByRole("tab", { name: "Builder" }));
    await expect(
      canvas.getByRole("heading", { name: "Preview" }),
    ).toBeVisible();
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

function draftEditor(
  canvas: Parameters<NonNullable<Story["play"]>>[0]["canvas"],
) {
  return canvas.getByRole("textbox", { name: "Daemon configuration YAML" });
}

async function reviewAndSave(canvas: Parameters<typeof draftEditor>[0]) {
  await userEvent.click(canvas.getByRole("button", { name: "Review changes" }));
  await userEvent.click(
    canvas.getByRole("button", { name: "Save configuration" }),
  );
}

const draftYaml = "programs: {}\n";

export const KeepsDraftAfterConflict: Story = {
  args: {
    onSave: fn<Save>(async ({ base_revision }) => {
      if (base_revision === 7) throw new ApiError(409, "revision_conflict");
      return { ...active, revision: 9, yaml: draftYaml };
    }),
    onLoadLatest: fn(async () => ({
      ...active,
      revision: 8,
      yaml: "inventoryInterval: 30m\nprograms: {}\n",
    })),
  },
  play: async ({ args, canvas }) => {
    fireEvent.change(draftEditor(canvas), { target: { value: draftYaml } });
    await reviewAndSave(canvas);
    await expect(await canvas.findByRole("alert")).toHaveTextContent(
      "Revision 8 is newer than your draft.",
    );
    await userEvent.click(
      canvas.getByRole("button", { name: "Keep my draft" }),
    );
    await expect(draftEditor(canvas)).toHaveValue(draftYaml);
    await reviewAndSave(canvas);
    await expect(args.onSave).toHaveBeenLastCalledWith({
      base_revision: 8,
      yaml: draftYaml,
    });
    await expect(await canvas.findByText("Revision 9 saved")).toBeVisible();
  },
};

export const ShowsInvalidYamlWithoutLosingDraft: Story = {
  args: {
    onSave: fn<Save>(async () => {
      throw new ApiError(422, "invalid_daemon_config", "Unclosed sequence");
    }),
  },
  play: async ({ canvas }) => {
    fireEvent.change(draftEditor(canvas), { target: { value: "programs: [" } });
    await reviewAndSave(canvas);
    await expect(await canvas.findByRole("alert")).toHaveTextContent(
      "Unclosed sequence",
    );
    await expect(draftEditor(canvas)).toHaveValue("programs: [");
    await expect(
      canvas.queryByText(/Revision \d+ saved/),
    ).not.toBeInTheDocument();
  },
};

export const OffersFreshSignIn: Story = {
  args: {
    onSave: fn<Save>(async () => {
      throw new ApiError(403, "reauthentication_required");
    }),
    onReauthenticate: fn(),
  },
  play: async ({ args, canvas }) => {
    fireEvent.change(draftEditor(canvas), { target: { value: draftYaml } });
    await reviewAndSave(canvas);
    await userEvent.click(
      await canvas.findByRole("button", { name: "Sign in again" }),
    );
    await expect(args.onReauthenticate).toHaveBeenCalledOnce();
    await expect(draftEditor(canvas)).toHaveValue(draftYaml);
  },
};

export const RestoresRevisionAsADraft: Story = {
  args: {
    selectedRevision: 1,
    selectedRevisionData: { ...history[0], revision: 1, yaml: draftYaml },
    onSave: fn<Save>(async () => ({
      ...active,
      revision: 8,
      yaml: draftYaml,
    })),
  },
  play: async ({ args, canvas }) => {
    await userEvent.click(
      canvas.getByRole("button", { name: "Restore to draft" }),
    );
    await expect(draftEditor(canvas)).toHaveValue(draftYaml);
    await expect(args.onSave).not.toHaveBeenCalled();
    await reviewAndSave(canvas);
    await expect(args.onSave).toHaveBeenCalledWith({
      base_revision: 7,
      yaml: draftYaml,
    });
    await expect(await canvas.findByText("Revision 8 saved")).toBeVisible();
  },
};
