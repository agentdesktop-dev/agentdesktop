import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fireEvent, spyOn, userEvent, waitFor } from "storybook/test";

import { App } from "./App";
import { navigate } from "./router";
import { macDevice } from "./stories/fixtures";
import type { ActiveDaemonConfig, AdminSession } from "./types";

const session: AdminSession = {
  mode: "local",
  authenticated: true,
  role: "operator",
  subject: null,
  assurance: null,
  recent_auth_required: false,
};

const active: ActiveDaemonConfig = {
  config: { programs: {} },
  yaml: "programs: {}\n",
  revision: 7,
  sha256: "a".repeat(64),
  source: "database",
  writable: true,
  created_at: null,
  author: null,
  message: null,
  reload_error: null,
};

let reported = false;
let devicesFail = false;
let activeRequests = 0;

const meta = {
  title: "Controller/Configuration page",
  component: App,
  parameters: { layout: "fullscreen" },
  tags: ["test"],
  beforeEach: () => {
    const previousUrl = window.location.href;
    const previousState = window.history.state;
    window.history.replaceState(previousState, "", "/configuration");
    reported = false;
    devicesFail = false;
    activeRequests = 0;
    const interval = globalThis.setInterval.bind(globalThis);
    const timer = spyOn(globalThis, "setInterval").mockImplementation(
      (handler, timeout, ...args) =>
        interval(handler, timeout === 10_000 ? 50 : timeout, ...args),
    );
    const fetch = spyOn(globalThis, "fetch").mockImplementation(
      async (input, init) => {
        const path = new URL(String(input), window.location.origin).pathname;
        if (path === "/auth/session") return Response.json(session);
        if (path === "/api/v1/devices") {
          if (devicesFail) {
            return Response.json(
              { error: "devices_unavailable" },
              { status: 503 },
            );
          }
          return Response.json([
            {
              ...macDevice,
              config_revision: reported ? 8 : 7,
              config_state: 1,
            },
          ]);
        }
        if (path === "/api/v1/daemon-config/revisions") {
          return Response.json({ revisions: [] });
        }
        if (path === "/api/v1/daemon-config" && init?.method === "PUT") {
          const input = JSON.parse(String(init.body)) as { yaml: string };
          return Response.json({ ...active, revision: 8, yaml: input.yaml });
        }
        if (path === "/api/v1/daemon-config") {
          activeRequests += 1;
          return Response.json(active);
        }
        return Response.json({ error: "not_found" }, { status: 404 });
      },
    );
    return () => {
      fetch.mockRestore();
      timer.mockRestore();
      window.history.replaceState(previousState, "", previousUrl);
    };
  },
} satisfies Meta<typeof App>;

export default meta;
type Story = StoryObj<typeof meta>;

export const RefreshesRolloutWithoutUnmountingEditor: Story = {
  play: async ({ canvas }) => {
    const editor = await canvas.findByRole("textbox", {
      name: "Daemon configuration YAML",
    });
    fireEvent.change(editor, { target: { value: "programs:\n  codex: {}\n" } });
    await userEvent.click(
      canvas.getByRole("button", { name: "Review changes" }),
    );
    await userEvent.click(
      canvas.getByRole("button", { name: "Save configuration" }),
    );
    await expect(await canvas.findByText("Revision 8 saved")).toBeVisible();
    await expect(canvas.getByText("1 not reported")).toBeVisible();
    reported = true;
    await expect(await canvas.findByText("1 applied")).toBeVisible();
    await expect(canvas.getByText("Revision 8 saved")).toBeVisible();
    await expect(activeRequests).toBe(1);
    devicesFail = true;
    await expect(
      await canvas.findByText(/Could not refresh device status/),
    ).toBeVisible();
    await expect(canvas.getByText("Revision 8 saved")).toBeVisible();
  },
};

export const GuardsBackAndForward: Story = {
  play: async ({ canvas }) => {
    await canvas.findByRole("textbox", { name: "Daemon configuration YAML" });
    navigate("/configuration?history=1");
    navigate("/configuration?history=2");
    window.history.back();
    await waitFor(() => expect(window.location.search).toBe("?history=1"));
    fireEvent.change(
      canvas.getByRole("textbox", { name: "Daemon configuration YAML" }),
      { target: { value: "programs:\n  codex: {}\n" } },
    );
    const confirm = spyOn(window, "confirm").mockReturnValue(false);
    try {
      window.history.back();
      await waitFor(() => expect(confirm).toHaveBeenCalledTimes(1));
      await waitFor(() => expect(window.location.search).toBe("?history=1"));
      window.history.forward();
      await waitFor(() => expect(confirm).toHaveBeenCalledTimes(2));
      await waitFor(() => expect(window.location.search).toBe("?history=1"));
      await expect(
        canvas.getByRole("textbox", { name: "Daemon configuration YAML" }),
      ).toHaveValue("programs:\n  codex: {}\n");
    } finally {
      confirm.mockRestore();
    }
  },
};
