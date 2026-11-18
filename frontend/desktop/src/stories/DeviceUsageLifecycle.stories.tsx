import type { Meta, StoryObj } from "@storybook/react-vite";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { useState } from "react";
import { expect, userEvent, waitFor, within } from "storybook/test";

import { useDeviceUsage } from "../useDeviceUsage";
import { deviceUsage } from "./fixtures";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

const requests: Array<ReturnType<typeof deferred<typeof deviceUsage>>> = [];

function DeviceUsageHarness() {
  const [active, setActive] = useState(true);
  const { error, loading, range, reload, setRange, usage } =
    useDeviceUsage(active);

  return (
    <main>
      <p data-testid="loading">{loading ? "loading" : "idle"}</p>
      <p data-testid="range">{range}</p>
      <p data-testid="usage">{usage ? "present" : "empty"}</p>
      <p data-testid="error">{error ?? "none"}</p>
      <button
        type="button"
        onClick={() => setRange(range === "7d" ? "30d" : "7d")}
      >
        Change range
      </button>
      <button type="button" onClick={() => void reload()}>
        Reload
      </button>
      <button type="button" onClick={() => setActive(false)}>
        Deactivate
      </button>
    </main>
  );
}

const meta = {
  component: DeviceUsageHarness,
  tags: ["test"],
} satisfies Meta<typeof DeviceUsageHarness>;

export default meta;
type Story = StoryObj<typeof meta>;

export const PreservesRequestLifecycle: Story = {
  beforeEach: () => {
    requests.length = 0;
    mockIPC((command) => {
      if (command !== "get_device_usage") {
        throw new Error(`Unexpected desktop command: ${command}`);
      }
      const request = deferred<typeof deviceUsage>();
      requests.push(request);
      return request.promise;
    });
    return () => clearMocks();
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const loading = canvas.getByTestId("loading");
    const usage = canvas.getByTestId("usage");
    const error = canvas.getByTestId("error");

    await waitFor(() => expect(requests).toHaveLength(1));
    await expect(loading).toHaveTextContent("loading");

    await userEvent.click(canvas.getByRole("button", { name: "Change range" }));
    await waitFor(() => expect(requests).toHaveLength(2));
    requests[0].resolve(deviceUsage);
    await waitFor(() => expect(usage).toHaveTextContent("empty"));
    await expect(loading).toHaveTextContent("loading");

    requests[1].resolve(deviceUsage);
    await waitFor(() => expect(usage).toHaveTextContent("present"));
    await userEvent.click(canvas.getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(requests).toHaveLength(3));
    requests[2].reject(new Error("offline"));
    await waitFor(() => expect(error).toHaveTextContent("offline"));
    await expect(usage).toHaveTextContent("present");

    await userEvent.click(canvas.getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(requests).toHaveLength(4));
    await userEvent.click(canvas.getByRole("button", { name: "Deactivate" }));
    await expect(loading).toHaveTextContent("idle");
    requests[3].resolve(deviceUsage);
    await waitFor(() => expect(error).toHaveTextContent("offline"));
    await expect(usage).toHaveTextContent("present");
  },
};
