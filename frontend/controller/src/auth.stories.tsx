import type { Meta, StoryObj } from "@storybook/react-vite";
import { useState } from "react";
import { expect, spyOn, userEvent } from "storybook/test";

import { ApiError, AuthProvider, useAuth } from "./auth";
import { ControllerShell } from "./components/ControllerShell";
import { SessionScreen } from "./components/SessionScreen";
import type { AdminSession } from "./types";

const oidc: AdminSession = {
  mode: "oidc",
  authenticated: true,
  role: "operator",
  subject: "admin@example.com",
  assurance: "mfa",
  recent_auth_required: true,
};

function Harness() {
  const auth = useAuth();
  const [result, setResult] = useState("");
  if (auth.loading || auth.error || !auth.session?.authenticated) {
    return (
      <SessionScreen
        loading={auth.loading}
        error={auth.error}
        onRetry={auth.retry}
      />
    );
  }
  return (
    <ControllerShell
      path="/"
      session={auth.session}
      onLogout={auth.logout}
      onRefresh={() => undefined}
    >
      <button
        type="button"
        className="button secondary"
        onClick={() => {
          void auth
            .request("/api/v1/devices/example", { method: "DELETE" })
            .then(() => setResult("Deleted"))
            .catch((error: unknown) =>
              setResult(error instanceof ApiError ? error.code : "failed"),
            );
        }}
      >
        Delete test device
      </button>
      <button
        type="button"
        className="button secondary"
        onClick={() => {
          void auth
            .request("/api/v1/devices")
            .then(() => setResult("Loaded"))
            .catch((error: unknown) =>
              setResult(error instanceof ApiError ? error.message : "failed"),
            );
        }}
      >
        Load devices
      </button>
      <p role="status">{result}</p>
    </ControllerShell>
  );
}

const meta = {
  title: "Controller/Authentication",
  component: Harness,
  decorators: [
    (Story) => (
      <AuthProvider>
        <Story />
      </AuthProvider>
    ),
  ],
  parameters: { layout: "fullscreen" },
  tags: ["test"],
} satisfies Meta<typeof Harness>;

export default meta;
type Story = StoryObj<typeof meta>;

function mockSession(session: AdminSession, status = 204, code = "") {
  return spyOn(globalThis, "fetch").mockImplementation(async (input, init) => {
    const path = new URL(String(input), window.location.origin).pathname;
    if (path === "/auth/session") return Response.json(session);
    if (path === "/auth/logout" || path === "/auth/activity")
      return new Response(null, { status: 204 });
    if (init?.method === "DELETE" || path === "/api/v1/devices") {
      return status === 204
        ? new Response(null, { status })
        : Response.json({ error: code }, { status });
    }
    return Response.json({ error: "not_found" }, { status: 404 });
  });
}

export const Anonymous: Story = {
  beforeEach: () => {
    const mock = mockSession({
      ...oidc,
      authenticated: false,
      role: null,
      subject: null,
    });
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await expect(
      await canvas.findByRole("link", { name: "Sign in" }),
    ).toHaveAttribute("href", "/auth/login");
    await expect(canvas.queryByRole("navigation")).not.toBeInTheDocument();
  },
};

export const OperatorMutationAndLogout: Story = {
  beforeEach: () => {
    const mock = mockSession(oidc);
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await expect(await canvas.findByText(oidc.subject as string)).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Delete test device" }),
    );
    await expect(await canvas.findByText("Deleted")).toBeVisible();
    await userEvent.click(canvas.getByRole("button", { name: "Sign out" }));
    await expect(
      await canvas.findByRole("link", { name: "Sign in" }),
    ).toBeVisible();
  },
};

export const ExpiredMutation: Story = {
  beforeEach: () => {
    const mock = mockSession(oidc, 401, "unauthenticated");
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await userEvent.click(
      await canvas.findByRole("button", { name: "Delete test device" }),
    );
    await expect(
      await canvas.findByRole("link", { name: "Sign in" }),
    ).toBeVisible();
  },
};

export const ReauthenticationResponse: Story = {
  beforeEach: () => {
    const mock = mockSession(oidc, 403, "reauthentication_required");
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await userEvent.click(
      await canvas.findByRole("button", { name: "Delete test device" }),
    );
    await expect(
      await canvas.findByText("reauthentication_required"),
    ).toBeVisible();
    await expect(
      canvas.getByRole("button", { name: "Sign out" }),
    ).toBeVisible();
  },
};

export const LocalMode: Story = {
  beforeEach: () => {
    const mock = mockSession({
      ...oidc,
      mode: "local",
      subject: null,
    });
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await expect(await canvas.findByText("Local mode")).toBeVisible();
    await expect(
      canvas.queryByRole("button", { name: "Sign out" }),
    ).not.toBeInTheDocument();
    await userEvent.click(
      canvas.getByRole("button", { name: "Delete test device" }),
    );
    await expect(await canvas.findByText("Deleted")).toBeVisible();
  },
};

export const ForbiddenMutation: Story = {
  beforeEach: () => {
    const mock = mockSession({ ...oidc, role: "viewer" }, 403, "forbidden");
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await userEvent.click(
      await canvas.findByRole("button", { name: "Delete test device" }),
    );
    await expect(await canvas.findByText("forbidden")).toBeVisible();
    await expect(canvas.getByText("viewer")).toBeVisible();
  },
};

export const SessionUnauthorized: Story = {
  beforeEach: () => {
    const mock = spyOn(globalThis, "fetch").mockResolvedValue(
      Response.json({ error: "unauthenticated" }, { status: 401 }),
    );
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await expect(
      await canvas.findByRole("link", { name: "Sign in" }),
    ).toBeVisible();
  },
};

export const SessionCheckRetry: Story = {
  beforeEach: () => {
    const mock = mockSession(oidc);
    mock.mockRejectedValueOnce(new Error("offline"));
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await expect(await canvas.findByRole("alert")).toBeVisible();
    await userEvent.click(canvas.getByRole("button", { name: "Try again" }));
    await expect(await canvas.findByText(oidc.subject as string)).toBeVisible();
  },
};

export const PassiveAndSyntheticEventsAreNotActivity: Story = {
  beforeEach: () => {
    const mock = mockSession(oidc);
    return () => mock.mockRestore();
  },
  play: async ({ canvas, canvasElement }) => {
    await canvas.findByRole("button", { name: "Delete test device" });
    const doc = canvasElement.ownerDocument;
    doc.dispatchEvent(new Event("pointerdown"));
    doc.dispatchEvent(new Event("keydown"));
    doc.dispatchEvent(new Event("wheel"));
    const calls = (
      globalThis.fetch as unknown as {
        mock: { calls: [unknown, RequestInit][] };
      }
    ).mock.calls;
    await expect(calls).toHaveLength(1);
    await expect(calls[0]?.[0]).toBe("/auth/session");
  },
};

export const ExpiredRead: Story = {
  beforeEach: () => {
    const mock = mockSession(oidc, 401, "unauthenticated");
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await userEvent.click(
      await canvas.findByRole("button", { name: "Load devices" }),
    );
    await expect(
      await canvas.findByRole("link", { name: "Sign in" }),
    ).toBeVisible();
  },
};

export const ProviderUnavailable: Story = {
  beforeEach: () => {
    const mock = mockSession(oidc, 503, "identity_provider_unavailable");
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await userEvent.click(
      await canvas.findByRole("button", { name: "Load devices" }),
    );
    await expect(
      await canvas.findByText(
        "The identity provider is unavailable. Try again shortly.",
      ),
    ).toBeVisible();
  },
};

export const SessionProviderUnavailable: Story = {
  beforeEach: () => {
    const mock = spyOn(globalThis, "fetch").mockResolvedValue(
      Response.json(
        { error: "identity_provider_unavailable" },
        { status: 503 },
      ),
    );
    return () => mock.mockRestore();
  },
  play: async ({ canvas }) => {
    await expect(
      await canvas.findByText(
        "The identity provider is unavailable. Try again shortly.",
      ),
    ).toBeVisible();
  },
};
