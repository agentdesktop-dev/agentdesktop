import agentdesktopLogo from "@brand/logo.svg";
import agentdesktopMark from "@brand/mark.svg";
import {
  Gauge,
  Laptop,
  RefreshCw,
  Settings,
  SlidersHorizontal,
} from "lucide-react";
import type { ReactNode } from "react";

import { Link } from "../router";

const navigation = [
  { href: "/", label: "Overview", icon: Gauge },
  { href: "/devices", label: "Devices", icon: Laptop },
  { href: "/configuration", label: "Configuration", icon: SlidersHorizontal },
  { href: "/settings", label: "Settings", icon: Settings },
];

export function ControllerShell({
  children,
  onRefresh,
  path,
}: {
  children: ReactNode;
  onRefresh: () => void;
  path: string;
}) {
  const pageTitle = path.startsWith("/devices/")
    ? "Device details"
    : (navigation.find((item) => item.href === path)?.label ?? "Overview");

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <img
            className="brand-logo"
            src={agentdesktopLogo}
            alt="Agentdesktop"
          />
          <img className="brand-icon" src={agentdesktopMark} alt="" />
        </div>
        <nav className="primary-nav" aria-label="Primary navigation">
          {navigation.map((item) => {
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
          <button type="button" className="refresh-button" onClick={onRefresh}>
            <RefreshCw size={14} /> Refresh
          </button>
        </header>
        <div className="page-content">{children}</div>
      </main>
    </div>
  );
}
