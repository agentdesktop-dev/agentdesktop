import { useTheme } from "@agentdesktop/ui";
import agentdesktopLogo from "@brand/logo.svg";
import agentdesktopLogoLight from "@brand/logo-light.svg";
import agentdesktopMark from "@brand/mark.svg";
import {
  ChartColumn,
  Gauge,
  Laptop,
  RefreshCw,
  ScrollText,
  Settings,
  SlidersHorizontal,
} from "lucide-react";
import { type ReactNode, useState } from "react";
import type { ControllerExtension } from "../extensions";
import { Link } from "../router";
import type { AdminSession } from "../types";

const navigation: Array<{
  href: string;
  label: string;
  icon: typeof Gauge;
  permission?: string;
}> = [
  { href: "/", label: "Overview", icon: Gauge },
  { href: "/devices", label: "Devices", icon: Laptop },
  { href: "/usage", label: "Usage", icon: ChartColumn },
  { href: "/configuration", label: "Configuration", icon: SlidersHorizontal },
  {
    href: "/audit",
    label: "Audit",
    icon: ScrollText,
    permission: "audit.read",
  },
  { href: "/settings", label: "Settings", icon: Settings },
];

export function ControllerShell({
  children,
  onRefresh,
  path,
  session,
  onLogout,
  extensions = [],
}: {
  children: ReactNode;
  onRefresh: () => void;
  path: string;
  session?: AdminSession;
  onLogout?: () => Promise<void>;
  extensions?: ControllerExtension[];
}) {
  const { theme } = useTheme();
  const [loggingOut, setLoggingOut] = useState(false);
  const items = [
    // Sessions that do not report permissions keep the full navigation.
    ...navigation.filter(
      (item) =>
        !item.permission ||
        !session?.permissions ||
        session.permissions.includes(item.permission),
    ),
    ...extensions.map((extension) => ({
      href: extension.path,
      label: extension.label,
      icon: extension.icon,
    })),
  ];
  const pageTitle = path.startsWith("/devices/")
    ? "Device details"
    : (items.find((item) => item.href === path)?.label ?? "Overview");

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <img
            className="brand-logo"
            src={theme === "dark" ? agentdesktopLogoLight : agentdesktopLogo}
            alt="Agentdesktop"
          />
          <img className="brand-icon" src={agentdesktopMark} alt="" />
        </div>
        <nav className="primary-nav" aria-label="Primary navigation">
          {items.map((item) => {
            const active =
              item.href === "/" ? path === "/" : path.startsWith(item.href);
            return (
              <Link
                href={item.href}
                className={`nav-item ${active ? "active" : ""}`}
                ariaLabel={item.label}
                ariaCurrent={active ? "page" : undefined}
                key={item.href}
              >
                <item.icon size={18} />
                <span>{item.label}</span>
              </Link>
            );
          })}
        </nav>
      </aside>

      <main className="main-area">
        <header className="topbar">
          <h1>{pageTitle}</h1>
          <div className="session-actions">
            {session && (
              <div className="session-identity">
                <span>
                  {session.mode === "local" ? "Local mode" : session.subject}
                </span>
                <span className="badge neutral">{session.role}</span>
                {session.mode !== "local" && onLogout && (
                  <button
                    type="button"
                    className="refresh-button"
                    disabled={loggingOut}
                    onClick={() => {
                      if (loggingOut) return;
                      setLoggingOut(true);
                      void onLogout().finally(() => setLoggingOut(false));
                    }}
                  >
                    {loggingOut ? "Signing out…" : "Sign out"}
                  </button>
                )}
              </div>
            )}
            <button
              type="button"
              className="refresh-button"
              onClick={onRefresh}
            >
              <RefreshCw size={14} /> Refresh
            </button>
          </div>
        </header>
        <div className="page-content">{children}</div>
      </main>
    </div>
  );
}
