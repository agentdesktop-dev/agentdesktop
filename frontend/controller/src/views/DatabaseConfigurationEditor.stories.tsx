import type { Meta, StoryObj } from "@storybook/react-vite";
import {
  expect,
  fireEvent,
  fn,
  spyOn,
  userEvent,
  waitFor,
} from "storybook/test";
import { parse } from "yaml";
import { ApiError } from "../auth";
import { navigate } from "../router";
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

export const BuilderPreservesGatewayAuthentication: Story = {
  args: {
    active: {
      ...active,
      yaml: [
        "llmGateway:",
        "  url: https://gateway.example.com",
        "  authentication:",
        "    type: controllerJwt",
        "    audience: fleet",
        "    allowedClientIds: [claude-code]",
        "  githubOAuth:",
        "    source: request",
        "programs: {}",
      ].join("\n"),
    },
  },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByRole("tab", { name: "Builder" }));
    await userEvent.click(canvas.getByText("Telemetry"));
    await userEvent.click(canvas.getByText("New session"));
    fireEvent.change(canvas.getByLabelText("JWT audience"), {
      target: { value: "updated-fleet" },
    });
    await userEvent.click(
      canvas.getByRole("button", { name: "Apply to draft" }),
    );
    const result = parse((draftEditor(canvas) as HTMLTextAreaElement).value);
    await expect(result.llmGateway.authentication.allowedClientIds).toEqual([
      "claude-code",
    ]);
    await expect(result.llmGateway.authentication.audience).toBe(
      "updated-fleet",
    );
    await expect(result.llmGateway.githubOAuth).toEqual({ source: "request" });
    await expect(result.telemetry.events).toEqual(["session.new"]);
  },
};

export const BuilderPreservesOidc: Story = {
  args: {
    active: {
      ...active,
      yaml: [
        "llmGateway:",
        "  url: https://gateway.example.com",
        "  authentication:",
        "    type: oidc",
        "    issuer: https://issuer.example.com",
        "    clientId: client",
        "programs: {}",
      ].join("\n"),
    },
  },
  play: async ({ canvas }) => {
    await userEvent.click(canvas.getByRole("tab", { name: "Builder" }));
    fireEvent.change(canvas.getByLabelText("Gateway URL"), {
      target: { value: "https://new.example.com" },
    });
    await userEvent.click(
      canvas.getByRole("button", { name: "Apply to draft" }),
    );
    const result = parse((draftEditor(canvas) as HTMLTextAreaElement).value);
    await expect(result.llmGateway.url).toBe("https://new.example.com");
    await expect(result.llmGateway.authentication).toEqual({
      type: "oidc",
      issuer: "https://issuer.example.com",
      clientId: "client",
    });
  },
};

export const InvalidBuilderInputKeepsYaml: Story = {
  play: async ({ canvas }) => {
    const invalid = "sandbox:\n  network:\n    allowedDomains: example.com\n";
    fireEvent.change(draftEditor(canvas), { target: { value: invalid } });
    await userEvent.click(canvas.getByRole("tab", { name: "Builder" }));
    await expect(canvas.getByRole("alert")).toHaveTextContent(
      "sandbox.network.allowedDomains must be a list of strings",
    );
    await userEvent.click(canvas.getByRole("tab", { name: "YAML" }));
    await expect(draftEditor(canvas)).toHaveValue(invalid);
    const cyclic = "programs:\n  claudeCode: &loop\n    recursive: *loop\n";
    fireEvent.change(draftEditor(canvas), { target: { value: cyclic } });
    await userEvent.click(canvas.getByRole("tab", { name: "Builder" }));
    await expect(canvas.getByRole("alert")).toHaveTextContent(
      "Cyclic YAML aliases cannot be edited",
    );
    await userEvent.click(canvas.getByRole("tab", { name: "YAML" }));
    await expect(draftEditor(canvas)).toHaveValue(cyclic);
  },
};

export const WarnsBeforeDiscardingBuilderChanges: Story = {
  play: async ({ canvas }) => {
    const confirm = spyOn(window, "confirm").mockReturnValue(false);
    try {
      await userEvent.click(canvas.getByRole("tab", { name: "Builder" }));
      await userEvent.click(canvas.getByText("Telemetry"));
      await userEvent.click(canvas.getByText("New session"));
      await expect(canvas.getByText("Unsaved")).toBeVisible();
      const beforeUnload = new Event("beforeunload", { cancelable: true });
      window.dispatchEvent(beforeUnload);
      await expect(beforeUnload.defaultPrevented).toBe(true);
      navigate("/devices");
      await expect(confirm).toHaveBeenCalledOnce();
      await userEvent.click(canvas.getByRole("tab", { name: "YAML" }));
      await expect(confirm).toHaveBeenCalledTimes(2);
      await expect(
        canvas.getByRole("tab", { name: "Builder" }),
      ).toHaveAttribute("aria-selected", "true");
      await expect(
        canvas.getByRole("checkbox", { name: /New session/ }),
      ).toBeChecked();
      confirm.mockReturnValue(true);
      await userEvent.click(canvas.getByRole("tab", { name: "YAML" }));
      await expect(draftEditor(canvas)).toHaveValue(yaml);
    } finally {
      confirm.mockRestore();
    }
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

let finishSave: ((value: ActiveDaemonConfig) => void) | undefined;

export const KeepsEditsMadeDuringSave: Story = {
  args: {
    onSave: fn<Save>(
      () =>
        new Promise<ActiveDaemonConfig>((resolve) => {
          finishSave = resolve;
        }),
    ),
  },
  play: async ({ canvas }) => {
    fireEvent.change(draftEditor(canvas), { target: { value: draftYaml } });
    await userEvent.click(
      canvas.getByRole("button", { name: "Review changes" }),
    );
    await userEvent.click(canvas.getByText("Add revision note"));
    const note = canvas.getByRole("textbox", { name: "Revision note" });
    fireEvent.change(note, { target: { value: "first note" } });
    await userEvent.click(
      canvas.getByRole("button", { name: "Save configuration" }),
    );
    const nextDraft = "inventoryInterval: 30m\nprograms: {}\n";
    fireEvent.change(draftEditor(canvas), { target: { value: nextDraft } });
    fireEvent.change(note, { target: { value: "next note" } });
    finishSave?.({ ...active, revision: 8, yaml: draftYaml });
    await expect(await canvas.findByText("Revision 8 saved")).toBeVisible();
    await expect(draftEditor(canvas)).toHaveValue(nextDraft);
    await userEvent.click(
      canvas.getByRole("button", { name: "Review changes" }),
    );
    await userEvent.click(canvas.getByText("Edit revision note"));
    await expect(
      canvas.getByRole("textbox", { name: "Revision note" }),
    ).toHaveValue("next note");
    await waitFor(() => expect(canvas.getByText("Unsaved")).toBeVisible());
  },
};

export const KeepsBuilderEditsMadeDuringSave: Story = {
  args: {
    onSave: fn<Save>(
      () =>
        new Promise<ActiveDaemonConfig>((resolve) => {
          finishSave = resolve;
        }),
    ),
  },
  play: async ({ canvas }) => {
    fireEvent.change(draftEditor(canvas), { target: { value: draftYaml } });
    await reviewAndSave(canvas);
    await userEvent.click(canvas.getByRole("tab", { name: "Builder" }));
    await userEvent.click(canvas.getByText("Telemetry"));
    await userEvent.click(canvas.getByText("New session"));
    finishSave?.({ ...active, revision: 8, yaml: draftYaml });
    await expect(await canvas.findByText("Revision 8 saved")).toBeVisible();
    await expect(
      canvas.getByRole("checkbox", { name: /New session/ }),
    ).toBeChecked();
    await expect(canvas.getByText("Unsaved")).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Apply to draft" }),
    );
    const result = parse((draftEditor(canvas) as HTMLTextAreaElement).value);
    await expect(result.telemetry.events).toEqual(["session.new"]);
  },
};

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
  },
  play: async ({ canvas }) => {
    fireEvent.change(draftEditor(canvas), { target: { value: draftYaml } });
    await userEvent.click(
      canvas.getByRole("button", { name: "Review changes" }),
    );
    await userEvent.click(canvas.getByText("Add revision note"));
    fireEvent.change(canvas.getByRole("textbox", { name: "Revision note" }), {
      target: { value: "Keep this note" },
    });
    await userEvent.click(
      canvas.getByRole("button", { name: "Save configuration" }),
    );
    const link = await canvas.findByRole("link", {
      name: "Sign in again in a new tab, then return to save",
    });
    await expect(link).toHaveAttribute("target", "_blank");
    await expect(link).toHaveAttribute(
      "href",
      "/auth/login?reauthenticate=true",
    );
    await expect(draftEditor(canvas)).toHaveValue(draftYaml);
    await expect(
      canvas.getByRole("textbox", { name: "Revision note" }),
    ).toHaveValue("Keep this note");
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
