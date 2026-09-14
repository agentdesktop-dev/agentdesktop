import { BrowserThemeProvider, SYSTEM_THEME_QUERY } from "@agentdesktop/ui";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { act, StrictMode, useState } from "react";
import { expect, userEvent } from "storybook/test";

import { SettingsView } from "../views/SettingsView";
import { ControllerStoryFrame } from "./ControllerStoryFrame";
import { controllerSettings } from "./fixtures";

function storageKey(id: string) {
  return `agentdesktop.storybook.appearance.${id}`;
}

async function storageChanged(key: string | null, newValue: string | null) {
  await act(async () => {
    window.dispatchEvent(
      new StorageEvent("storage", { key, newValue, storageArea: localStorage }),
    );
  });
}

let changeSystemTheme = (_dark: boolean) => {};

function AppearanceFixture({ id }: { id: string }) {
  const [instance, setInstance] = useState(0);
  return (
    <StrictMode>
      <BrowserThemeProvider key={instance} storageKey={storageKey(id)}>
        <ControllerStoryFrame path="/settings">
          <SettingsView data={controllerSettings} />
          <div className="dialog-actions">
            <button
              className="button secondary"
              type="button"
              onClick={() => changeSystemTheme(true)}
            >
              Simulate dark system
            </button>
            <button
              className="button secondary"
              type="button"
              onClick={() => changeSystemTheme(false)}
            >
              Simulate light system
            </button>
            <button
              className="button secondary"
              type="button"
              onClick={() => setInstance((value) => value + 1)}
            >
              Remount preview
            </button>
          </div>
        </ControllerStoryFrame>
      </BrowserThemeProvider>
    </StrictMode>
  );
}

const meta = {
  title: "Controller/Appearance behavior",
  tags: ["test"],
  parameters: { themeProvider: false },
  args: {
    initialValue: null as string | null,
    blockStorage: false,
  },
  beforeEach: ({ id, args }) => {
    const key = storageKey(id);
    const storage = window.localStorage;
    storage.removeItem(key);
    if (args.initialValue !== null) storage.setItem(key, args.initialValue);

    const originalMatchMedia = window.matchMedia;
    const media = originalMatchMedia.call(window, SYSTEM_THEME_QUERY);
    let dark = false;
    Object.defineProperty(media, "matches", { get: () => dark });
    window.matchMedia = (query) =>
      query === SYSTEM_THEME_QUERY
        ? media
        : originalMatchMedia.call(window, query);
    changeSystemTheme = (next) => {
      dark = next;
      media.dispatchEvent(new Event("change"));
    };

    const storageDescriptor = Object.getOwnPropertyDescriptor(
      window,
      "localStorage",
    );
    if (args.blockStorage) {
      Object.defineProperty(window, "localStorage", {
        configurable: true,
        get() {
          throw new DOMException("Storage is blocked", "SecurityError");
        },
      });
    }
    return () => {
      window.matchMedia = originalMatchMedia;
      if (storageDescriptor) {
        Object.defineProperty(window, "localStorage", storageDescriptor);
      }
      storage.removeItem(key);
    };
  },
  render: (_args, { id }) => <AppearanceFixture id={id} />,
} satisfies Meta<{ initialValue: string | null; blockStorage: boolean }>;

export default meta;
type Story = StoryObj<typeof meta>;

export const DefaultsToSystemAndTracksChanges: Story = {
  play: async ({ canvas, canvasElement, id }) => {
    const root = canvasElement.ownerDocument.documentElement;
    const select = canvas.getByRole("combobox", { name: "Color mode" });
    await expect(select).toHaveValue("system");
    await expect(root).toHaveAttribute("data-theme", "light");
    await expect(localStorage.getItem(storageKey(id))).toBeNull();
    await userEvent.click(
      canvas.getByRole("button", { name: "Simulate dark system" }),
    );
    await expect(root).toHaveAttribute("data-theme", "dark");
    await expect(select).toHaveValue("system");
    await expect(localStorage.getItem(storageKey(id))).toBeNull();
    await userEvent.click(
      canvas.getByRole("button", { name: "Simulate light system" }),
    );
    await expect(root).toHaveAttribute("data-theme", "light");
  },
};

export const ExplicitModesIgnoreSystemChanges: Story = {
  play: async ({ canvas, canvasElement }) => {
    const root = canvasElement.ownerDocument.documentElement;
    const select = canvas.getByRole("combobox", { name: "Color mode" });
    await userEvent.selectOptions(select, "light");
    await userEvent.click(
      canvas.getByRole("button", { name: "Simulate dark system" }),
    );
    await expect(root).toHaveAttribute("data-theme", "light");
    await userEvent.selectOptions(select, "dark");
    await userEvent.click(
      canvas.getByRole("button", { name: "Simulate light system" }),
    );
    await expect(root).toHaveAttribute("data-theme", "dark");
    await userEvent.selectOptions(select, "system");
    await expect(root).toHaveAttribute("data-theme", "light");
    await userEvent.click(
      canvas.getByRole("button", { name: "Simulate dark system" }),
    );
    await expect(root).toHaveAttribute("data-theme", "dark");
  },
};

export const PersistsAndRestoresSelection: Story = {
  play: async ({ canvas, canvasElement, id }) => {
    await userEvent.selectOptions(
      canvas.getByRole("combobox", { name: "Color mode" }),
      "dark",
    );
    await expect(localStorage.getItem(storageKey(id))).toBe("dark");
    await userEvent.click(
      canvas.getByRole("button", { name: "Remount preview" }),
    );
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toHaveValue("dark");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "dark",
    );
  },
};

export const RestoresSavedDarkMode: Story = {
  args: { initialValue: "dark" },
  play: async ({ canvas, canvasElement }) => {
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toHaveValue("dark");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "dark",
    );
  },
};

export const InvalidPreferenceFallsBackToSystem: Story = {
  args: { initialValue: "invalid-mode" },
  play: async ({ canvas, canvasElement }) => {
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toHaveValue("system");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "light",
    );
  },
};

export const SynchronizesOtherTabs: Story = {
  play: async ({ canvas, canvasElement, id }) => {
    const root = canvasElement.ownerDocument.documentElement;
    const select = canvas.getByRole("combobox", { name: "Color mode" });
    localStorage.setItem(storageKey(id), "dark");
    await storageChanged(storageKey(id), "dark");
    await expect(select).toHaveValue("dark");
    await expect(root).toHaveAttribute("data-theme", "dark");
    await storageChanged("unrelated-preference", "light");
    await expect(root).toHaveAttribute("data-theme", "dark");
    localStorage.removeItem(storageKey(id));
    await storageChanged(null, null);
    await expect(select).toHaveValue("system");
    await expect(root).toHaveAttribute("data-theme", "light");
  },
};

export const IgnoresStaleStorageEvents: Story = {
  play: async ({ canvas, canvasElement, id }) => {
    localStorage.setItem(storageKey(id), "dark");
    await userEvent.selectOptions(
      canvas.getByRole("combobox", { name: "Color mode" }),
      "light",
    );
    await storageChanged(storageKey(id), "dark");
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toHaveValue("light");
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "light",
    );
    await expect(localStorage.getItem(storageKey(id))).toBe("light");
  },
};

export const WorksWhenStorageIsBlocked: Story = {
  args: { blockStorage: true },
  play: async ({ canvas, canvasElement }) => {
    await expect(
      canvas.getByRole("combobox", { name: "Color mode" }),
    ).toHaveValue("system");
    await userEvent.selectOptions(
      canvas.getByRole("combobox", { name: "Color mode" }),
      "dark",
    );
    await expect(canvasElement.ownerDocument.documentElement).toHaveAttribute(
      "data-theme",
      "dark",
    );
    await expect(canvas.getByRole("status")).toHaveTextContent(
      "This browser couldn’t save your preference.",
    );
  },
};
