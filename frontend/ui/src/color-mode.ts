export type ColorMode = "system" | "light" | "dark";
export type Theme = Exclude<ColorMode, "system">;

export const COLOR_MODE_STORAGE_KEY = "agentdesktop.colorMode";
export const SYSTEM_THEME_QUERY = "(prefers-color-scheme: dark)";

export function parseColorMode(value: unknown): ColorMode {
  return value === "light" || value === "dark" ? value : "system";
}

export function systemTheme(): Theme {
  return typeof window !== "undefined" &&
    window.matchMedia(SYSTEM_THEME_QUERY).matches
    ? "dark"
    : "light";
}

export function readColorMode(storageKey: string): ColorMode {
  try {
    return parseColorMode(window.localStorage.getItem(storageKey));
  } catch {
    return "system";
  }
}

export function applyColorMode(
  mode: ColorMode,
  theme: Theme = mode === "system" ? systemTheme() : mode,
) {
  const root = document.documentElement;
  root.dataset.colorMode = mode;
  root.dataset.theme = theme;
  root.style.colorScheme = theme;
  const themeColor = document.querySelector<HTMLMetaElement>(
    'meta[name="theme-color"]',
  );
  const canvas = getComputedStyle(root).getPropertyValue("--canvas").trim();
  if (themeColor && canvas) themeColor.content = canvas;
}
