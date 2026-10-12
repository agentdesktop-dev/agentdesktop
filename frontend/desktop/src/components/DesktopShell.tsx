import { useTheme } from "@agentdesktop/ui";
import agentdesktopLogo from "@brand/logo.svg";
import agentdesktopLogoLight from "@brand/logo-light.svg";
import agentdesktopMark from "@brand/mark.svg";
import {
  ChartColumn,
  Gauge,
  Laptop,
  LoaderCircle,
  RefreshCw,
  Settings,
} from "lucide-react";
import type { ReactNode } from "react";

import type { Notice, View } from "../useDesktopModel";

export interface DesktopShellProps {
  children: ReactNode;
  fullWidth: boolean;
  isRefreshing: boolean;
  notice: Notice;
  onNavigate: (view: View) => void;
  onRefresh: () => void;
  pageTitle: string;
  refreshError: string | null;
  /** Whether this device has controller usage to show. */
  showUsage?: boolean;
  view: View;
}

export function DesktopShell({
  children,
  fullWidth,
  isRefreshing,
  notice,
  onNavigate,
  onRefresh,
  pageTitle,
  refreshError,
  showUsage = false,
  view,
}: DesktopShellProps) {
  const { theme } = useTheme();
  return (
    <div
      className={fullWidth ? "desktop-shell enrollment-shell" : "desktop-shell"}
    >
      {!fullWidth ? (
        <aside className="desktop-sidebar">
          <div className="desktop-brand">
            <img
              className="desktop-brand-logo"
              src={theme === "dark" ? agentdesktopLogoLight : agentdesktopLogo}
              alt="Agentdesktop"
            />
            <img className="desktop-brand-icon" src={agentdesktopMark} alt="" />
          </div>
          <nav className="desktop-nav" aria-label="Application">
            <button
              type="button"
              className={view === "home" ? "active" : ""}
              aria-current={view === "home" ? "page" : undefined}
              onClick={() => onNavigate("home")}
            >
              <Gauge size={18} />
              <span className="desktop-nav-label">Status</span>
            </button>
            <button
              type="button"
              className={view === "tools" ? "active" : ""}
              aria-current={view === "tools" ? "page" : undefined}
              onClick={() => onNavigate("tools")}
            >
              <Laptop size={18} />
              <span className="desktop-nav-label">Tools</span>
            </button>
            {showUsage ? (
              <button
                type="button"
                className={view === "usage" ? "active" : ""}
                aria-current={view === "usage" ? "page" : undefined}
                onClick={() => onNavigate("usage")}
              >
                <ChartColumn size={18} />
                <span className="desktop-nav-label">Usage</span>
              </button>
            ) : null}
            <button
              type="button"
              className={view === "settings" ? "active" : ""}
              aria-current={view === "settings" ? "page" : undefined}
              onClick={() => onNavigate("settings")}
            >
              <Settings size={18} />
              <span className="desktop-nav-label">Settings</span>
            </button>
          </nav>
        </aside>
      ) : null}

      <section className="desktop-main">
        <header className="desktop-page-header">
          <h1>{pageTitle}</h1>
          <div className="desktop-header-actions">
            {fullWidth ? (
              <button
                className="desktop-refresh"
                type="button"
                onClick={() => onNavigate("settings")}
              >
                <Settings size={14} /> Settings
              </button>
            ) : null}
            <button
              className="desktop-refresh"
              type="button"
              onClick={onRefresh}
              disabled={isRefreshing}
            >
              {isRefreshing ? (
                <LoaderCircle className="spin" size={14} />
              ) : (
                <RefreshCw size={14} />
              )}{" "}
              Refresh
            </button>
          </div>
        </header>
        <main className="desktop-content">
          {refreshError ? (
            <div className="notice notice-error" role="alert">
              {refreshError}
            </div>
          ) : null}
          {notice ? (
            <div
              className={`notice notice-${notice.tone}`}
              role={notice.tone === "error" ? "alert" : "status"}
            >
              {notice.message}
            </div>
          ) : null}
          {children}
        </main>
      </section>
    </div>
  );
}
