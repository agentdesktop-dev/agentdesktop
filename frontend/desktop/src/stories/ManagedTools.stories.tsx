import type { Meta, StoryObj } from "@storybook/react-vite";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { expect, fn, userEvent } from "storybook/test";

import { Desktop } from "../Desktop";
import type { ProgramChoice, ProgramManagement } from "../types";
import {
  allProgramsManaged,
  bootstrap,
  emptyDiscovery,
  standaloneConnector,
  unconfiguredDevice,
} from "./fixtures";

const refusal =
  "daemon returned 403 Forbidden: this device's policy manages every configured program (programSelection.allowUserChoice is off)";

let management: ProgramManagement;
const choiceRequest = fn();

const meta = {
  title: "Desktop/Managed tools behavior",
  component: Desktop,
  tags: ["test"],
  parameters: { themeProvider: false },
  beforeEach: ({ parameters }) => {
    management = allProgramsManaged;
    choiceRequest.mockClear();
    mockIPC((command, payload) => {
      switch (command) {
        case "desktop_ready":
          return;
        case "get_bootstrap":
          return bootstrap;
        case "get_connector_status":
          return standaloneConnector;
        case "get_managed_device_status":
          return unconfiguredDevice;
        case "get_discovery":
          return emptyDiscovery;
        case "get_program_management":
          return management;
        case "set_program_choice": {
          const { choice } = payload as { choice: ProgramChoice };
          choiceRequest(choice);
          if (parameters.choiceRefused) throw new Error(refusal);
          const managed = choice.mode !== "none";
          management = {
            ...management,
            choice,
            programs: management.programs.map((program) => ({
              program: program.program,
              managed,
              state: managed ? "applied" : "opted_out",
            })),
          };
          return management;
        }
        default:
          throw new Error(`Unexpected desktop command: ${command}`);
      }
    });
    return () => clearMocks();
  },
} satisfies Meta<typeof Desktop>;

export default meta;
type Story = StoryObj<typeof meta>;

export const OptsOutOfEveryTool: Story = {
  play: async ({ canvas }) => {
    await userEvent.click(
      await canvas.findByRole("button", { name: "Settings" }),
    );
    await expect(
      await canvas.findByRole("radio", { name: "All" }),
    ).toBeChecked();
    await userEvent.click(canvas.getByRole("radio", { name: "None" }));
    await expect(choiceRequest).toHaveBeenCalledWith({ mode: "none" });
    await expect(
      await canvas.findByText("Managed tools updated"),
    ).toBeVisible();
    await expect(canvas.getByRole("radio", { name: "None" })).toBeChecked();
    await expect(canvas.getAllByText("Opted out")).toHaveLength(3);
  },
};

export const ShowsTheDaemonRefusal: Story = {
  parameters: { choiceRefused: true },
  play: async ({ canvas }) => {
    await userEvent.click(
      await canvas.findByRole("button", { name: "Settings" }),
    );
    await userEvent.click(await canvas.findByRole("radio", { name: "None" }));
    await expect(await canvas.findByRole("alert")).toHaveTextContent(refusal);
    await expect(canvas.getByRole("radio", { name: "All" })).toBeChecked();
  },
};
