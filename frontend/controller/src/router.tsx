import type { PropsWithChildren } from "react";
import { useEffect, useState } from "react";

export function usePath() {
  const [path, setPath] = useState(window.location.pathname);
  useEffect(() => {
    const update = () => setPath(window.location.pathname);
    window.addEventListener("popstate", update);
    return () => window.removeEventListener("popstate", update);
  }, []);
  return path;
}

export function navigate(href: string) {
  window.history.pushState({}, "", href);
  window.dispatchEvent(new PopStateEvent("popstate"));
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
