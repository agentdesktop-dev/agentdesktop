import type { PropsWithChildren } from "react";
import { useEffect, useState } from "react";

let navigationGuard: (() => boolean) | null = null;
const historyIndexKey = "__agentdesktopHistoryIndex";

function historyIndex(state: unknown): number | null {
  if (
    state &&
    typeof state === "object" &&
    historyIndexKey in state &&
    typeof state[historyIndexKey] === "number"
  ) {
    return state[historyIndexKey];
  }
  return null;
}

function ensureHistoryIndex() {
  const index = historyIndex(window.history.state);
  if (index !== null) return index;
  window.history.replaceState(
    { ...window.history.state, [historyIndexKey]: 0 },
    "",
  );
  return 0;
}

export function registerNavigationGuard(guard: () => boolean) {
  navigationGuard = guard;
  return () => {
    if (navigationGuard === guard) navigationGuard = null;
  };
}

export function usePath() {
  const [path, setPath] = useState(window.location.pathname);
  useEffect(() => {
    let index = ensureHistoryIndex();
    let href = window.location.href;
    let restoring = false;
    const update = () => {
      index = ensureHistoryIndex();
      href = window.location.href;
      setPath(window.location.pathname);
    };
    const onPopState = (event: PopStateEvent) => {
      if (restoring) {
        restoring = false;
        update();
        return;
      }
      if (navigationGuard && !navigationGuard()) {
        const nextIndex = historyIndex(event.state);
        if (nextIndex !== null && nextIndex !== index) {
          restoring = true;
          window.history.go(index - nextIndex);
        } else {
          window.history.pushState({ [historyIndexKey]: index }, "", href);
        }
        return;
      }
      update();
    };
    window.addEventListener("popstate", onPopState);
    window.addEventListener("agentdesktop:navigate", update);
    return () => {
      window.removeEventListener("popstate", onPopState);
      window.removeEventListener("agentdesktop:navigate", update);
    };
  }, []);
  return path;
}

export function navigate(href: string) {
  if (navigationGuard && !navigationGuard()) return;
  const index = ensureHistoryIndex();
  window.history.pushState({ [historyIndexKey]: index + 1 }, "", href);
  window.dispatchEvent(new Event("agentdesktop:navigate"));
  window.scrollTo({ top: 0 });
}

export function Link({
  href,
  className,
  children,
  ariaLabel,
  ariaCurrent,
}: PropsWithChildren<{
  href: string;
  className?: string;
  ariaLabel?: string;
  ariaCurrent?: "page";
}>) {
  return (
    <a
      href={href}
      className={className}
      aria-label={ariaLabel}
      aria-current={ariaCurrent}
      onClick={(event) => {
        if (!event.metaKey && !event.ctrlKey && !event.shiftKey) {
          event.preventDefault();
          navigate(href);
        }
      }}
    >
      {children}
    </a>
  );
}
