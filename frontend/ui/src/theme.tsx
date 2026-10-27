import {
  createContext,
  type PropsWithChildren,
  useContext,
  useEffect,
  useId,
  useLayoutEffect,
  useState,
  useSyncExternalStore,
} from "react";

import {
  applyColorMode,
  COLOR_MODE_STORAGE_KEY,
  type ColorMode,
  parseColorMode,
  readColorMode,
  SYSTEM_THEME_QUERY,
  systemTheme,
  type Theme,
} from "./color-mode";

interface Appearance {
  colorMode: ColorMode;
  theme: Theme;
  setColorMode: (mode: ColorMode) => void;
  saveError?: string | null;
}

const ThemeContext = createContext<Appearance | null>(null);

function subscribeToSystemTheme(onChange: () => void) {
  const media = window.matchMedia(SYSTEM_THEME_QUERY);
  // Older WebKit versions expose only the legacy MediaQueryList listeners.
  if (media.addEventListener) {
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }
  media.addListener(onChange);
  return () => media.removeListener(onChange);
}

export function ThemeProvider({
  children,
  mode,
  defaultMode = "system",
  onModeChange,
  saveError,
}: PropsWithChildren<{
  mode?: ColorMode;
  defaultMode?: ColorMode;
  onModeChange?: (mode: ColorMode) => void;
  saveError?: string | null;
}>) {
  const [localMode, setLocalMode] = useState(defaultMode);
  const colorMode = mode ?? localMode;
  const preferredTheme = useSyncExternalStore(
    subscribeToSystemTheme,
    systemTheme,
    () => "light" as const,
  );
  const theme = colorMode === "system" ? preferredTheme : colorMode;

  useLayoutEffect(() => {
    applyColorMode(colorMode, theme);
  }, [colorMode, theme]);

  function setColorMode(next: ColorMode) {
    if (mode === undefined) setLocalMode(next);
    onModeChange?.(next);
  }

  return (
    <ThemeContext.Provider
      value={{ colorMode, theme, setColorMode, saveError }}
    >
      {children}
    </ThemeContext.Provider>
  );
}

export function BrowserThemeProvider({
  children,
  storageKey = COLOR_MODE_STORAGE_KEY,
}: PropsWithChildren<{ storageKey?: string }>) {
  const [mode, setMode] = useState(() => readColorMode(storageKey));
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    let storage: Storage;
    try {
      storage = window.localStorage;
    } catch {
      return;
    }
    function onStorage(event: StorageEvent) {
      if (
        event.storageArea === storage &&
        (event.key === storageKey || event.key === null)
      ) {
        // An event may describe an older write than the current stored value.
        setMode(readColorMode(storageKey));
        setSaveError(null);
      }
    }
    window.addEventListener("storage", onStorage);
    // Catch changes between the initial render and listener registration.
    setMode(readColorMode(storageKey));
    return () => window.removeEventListener("storage", onStorage);
  }, [storageKey]);

  function setColorMode(next: ColorMode) {
    setMode(next);
    try {
      window.localStorage.setItem(storageKey, next);
      setSaveError(null);
    } catch {
      setSaveError(
        "This browser couldn’t save your preference. It will reset when you reload.",
      );
    }
  }

  return (
    <ThemeProvider
      mode={mode}
      onModeChange={setColorMode}
      saveError={saveError}
    >
      {children}
    </ThemeProvider>
  );
}

export function useTheme(): Appearance {
  const appearance = useContext(ThemeContext);
  if (!appearance) throw new Error("Appearance requires a ThemeProvider");
  return appearance;
}

export function AppearancePreference({
  disabled = false,
  description = "System follows your device’s appearance.",
}: {
  disabled?: boolean;
  description?: string;
}) {
  const id = useId();
  const { colorMode, setColorMode, saveError } = useTheme();

  return (
    <div className="appearance-preference">
      <div>
        <label htmlFor={id}>Color mode</label>
        <p id={`${id}-description`}>{description}</p>
        {saveError ? <p role="status">{saveError}</p> : null}
      </div>
      <select
        id={id}
        aria-describedby={`${id}-description`}
        value={colorMode}
        disabled={disabled}
        onChange={(event) => setColorMode(parseColorMode(event.target.value))}
      >
        <option value="system">System</option>
        <option value="light">Light</option>
        <option value="dark">Dark</option>
      </select>
    </div>
  );
}
